# ESB PRX rx=0 根因分析：BLE 连接期间 RADIO 中断不触发

> 日期: 2026-06-09
> 分支: `feat/mpsl-timeslot`
> 最新 commit: `6ff4c9a` — Replace LeConnUpdate with L2CAP conn param update

## 1. 问题现象

PRX（`mpsl_3mode_central`）在 BLE 未连接时 ESB 正常收包。BLE 手机连接后：

- ✅ L2CAP conn param update accepted（100ms interval）
- ✅ MPSL BLOCKED rate 从 ~40/2s 降到 ~3/2s（timeslot 基本都能获得）
- ✅ `start > 0`、`timer0 > 0`（timeslot 正常授予，TIMER0 正常触发）
- ❌ **`radio = 0`、`rx = 0`** — SIGNAL_RADIO 从未触发，无任何 ESB 收包

## 2. 当前架构

### 中断绑定

```rust
// mpsl_3mode_central.rs
bind_interrupts!(struct Irqs {
    RADIO  => nrf_mpsl::HighPrioInterruptHandler;  // priority 0
    TIMER0 => nrf_mpsl::HighPrioInterruptHandler;  // priority 0
    RTC0   => nrf_mpsl::HighPrioInterruptHandler;  // priority 0
});
```

RADIO / TIMER0 / RTC0 全部由 MPSL C 库的 HighPrioInterruptHandler 接管（priority 0）。

### PRX timeslot callback 关键流程

```
SIGNAL_START → power-cycle RADIO → ESB init → start_receiving_manual_ack
             → NVIC::unmask(RADIO) ← ⚠️ 关键点
             → return ACTION_NONE

SIGNAL_RADIO → 检查 events_disabled → 处理接收/ACK → 重启 RX
             （从未到达）
```

### HFCLK 配置

```rust
// mpsl_timeslot.rs:58
const TIMESLOT_HFCLK: u8 = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
```

PRX 端（`mpsl_3mode_central`）没有调用 `mpsl.request_hfclk()`。PTX 端（`mpsl_3mode_event`）有调用。

## 3. 根因分析

### 🔴 根因 #1：NVIC::unmask(RADIO) 与 MPSL 内部中断管理冲突

**位置**: `src/mpsl_timeslot.rs:1793`（PRX）和 `src/mpsl_timeslot.rs:1014`（PTX）

```rust
// 当前代码 — 在 SIGNAL_START 回调末尾
unsafe {
    cortex_m::peripheral::NVIC::unmask(pac::Interrupt::RADIO);
}
```

**Nordic 官方明确禁止这种做法。** 来自 [DevZone Q&A](https://devzone.nordicsemi.com/f/nordic-q-a/91837):

> *Application code cannot register its own RADIO IRQ handler while MPSL is initialized.
> MPSL owns the RADIO vector at priority 0. RADIO events arrive via
> `MPSL_TIMESLOT_SIGNAL_RADIO` callback signal — you do NOT call
> `NVIC_EnableIRQ(RADIO_IRQn)` yourself.*

**Nordic 官方参考实现**（[micro-ESB + BLE 博文](https://devzone.nordicsemi.com/nordic/nordic-blog/b/blog/posts/running-micro-esb-concurrently-with-ble)）：

```c
case NRF_RADIO_CALLBACK_SIGNAL_TYPE_START:
    // 只配置 TIMER0 + RADIO power on
    NRF_RADIO->POWER = (RADIO_POWER_POWER_Enabled << RADIO_POWER_POWER_Pos);
    NVIC_EnableIRQ(TIMER0_IRQn);       // ✅ 只启用 TIMER0
    // ❌ 没有 NVIC_EnableIRQ(RADIO_IRQn)
    break;

case NRF_RADIO_CALLBACK_SIGNAL_TYPE_RADIO:
    RADIO_IRQHandler();                 // ✅ 在回调中直接调用 ESB IRQ handler
    break;
```

**冲突机制**：

1. MPSL C 库内部管理 RADIO NVIC 的 mask/unmask 状态
2. SDC BLE 连接事件期间，MPSL 频繁切换 RADIO 所有权
3. 应用层的 `NVIC::unmask(RADIO)` 破坏了 MPSL 内部状态机
4. 当 BLE 连接活跃时，MPSL 在 timeslot callback 返回后可能 re-mask RADIO NVIC
5. RADIO 硬件产生了 DISABLED event → NVIC pending → 但 MPSL 不转发 → SIGNAL_RADIO 不触发

**为什么 advertising-only 正常？** advertising 模式下 SDC 不做连接事件，MPSL 几乎不管理 RADIO NVIC，应用代码的 unmask 不会冲突。BLE 连接后冲突暴露。

### 🟠 根因 #2：HFCLK 未保证

**MPSL 头文件警告**（[mpsl_timeslot.h](https://github.com/nrfconnect/sdk-nrfxlib/blob/master/mpsl/include/mpsl_timeslot.h)）：

> *If the application will use the radio peripheral in timeslots with this configuration
> [NO_GUARANTEE], it must ensure that the crystal is running and stable before starting
> the radio.*

当前情况：
- PTX 端（`mpsl_3mode_event`）：✅ 有 `mpsl.request_hfclk()`
- PRX 端（`mpsl_3mode_central`）：❌ **没有** `request_hfclk()`

BLE 连接后，SDC 可能在连接事件间隙关闭 crystal。如果 timeslot 开始时 crystal 未运行，RADIO 可能无法正常工作。

### 🟡 根因 #3：缺少 NVIC::unpend(RADIO)

Nordic 官方 `nrf_esb.c` 在启动 TX 前总是：

```c
NVIC_ClearPendingIRQ(RADIO_IRQn);    // 清除残留 pending
NVIC_EnableIRQ(RADIO_IRQn);
```

当前代码没有 `NVIC::unpend(RADIO)`。SDC 结束 BLE 连接事件后可能留下 pending RADIO 中断。

### 🟡 根因 #4：在 priority 0 回调中做完整 RADIO init

Nordic 官方用**三层优先级**架构：

```
Priority 0 (LowerStack):  timeslot callback — 只做 TIMER0 配置 + RADIO power on
                          → 触发 software IRQ (LPCOMP_IRQn at priority 1)
Priority 1 (App IRQ):     TIMESLOT_BEGIN_IRQHandler — 完整 ESB init + start RX/TX
Priority 3 (App IRQ):     UESB_RX_HANDLE_IRQHandler — 通知应用层 RX data
```

当前代码在 SIGNAL_START（priority 0）中直接做完整的 RADIO power-cycle + ESB init + start receiving，这在 BLE 连接活跃时可能与 MPSL 内部状态转换时序冲突。

## 4. 官方参考对照

| 特征 | Nordic 官方（micro-ESB） | 当前 embassy 代码 | 影响 |
|------|------------------------|------------------|------|
| SIGNAL_START 中 NVIC RADIO | ❌ 不操作 | ✅ `NVIC::unmask(RADIO)` | **关键冲突** |
| SIGNAL_RADIO 处理 | 调用完整 `RADIO_IRQHandler()` | 内联简化处理 | 逻辑等效 |
| HFCLK 配置 | XTAL_GUARANTEED | NO_GUARANTEE | **未保证 crystal** |
| HFCLK 请求 | SDC 保证 | PRX 未请求 | **crystal 可能未运行** |
| NVIC unpend | `NVIC_ClearPendingIRQ()` | 缺失 | 残留中断干扰 |
| 初始化优先级 | 三层（P0→P1→P3） | 全部在 P0 | 可能时序问题 |
| Timeslot 结束清理 | 独立低优先级 handler | 在 TIMER0 回调中 | 等效 |

## 5. 修复方案

### 方案 A：最小修复（推荐先尝试）

#### 改动 1：移除 NVIC::unmask(RADIO)

**文件**: `src/mpsl_timeslot.rs`

PRX SIGNAL_START 回调（~行 1793）：
```rust
// 删除这两行：
unsafe {
    cortex_m::peripheral::NVIC::unmask(pac::Interrupt::RADIO);
}
```

PTX SIGNAL_START 回调（~行 1014）：
```rust
// 同样删除：
unsafe {
    cortex_m::peripheral::NVIC::unmask(pac::Interrupt::RADIO);
}
```

理由：MPSL 内部管理 RADIO NVIC。应用层不应干预。RADIO events 通过 SIGNAL_RADIO 回调传递，不需要手动 unmask。

#### 改动 2：HFCLK 改为 XTAL_GUARANTEED

**文件**: `src/mpsl_timeslot.rs`

```rust
// 行 58，改为：
const TIMESLOT_HFCLK: u8 = raw::MPSL_TIMESLOT_HFCLK_CFG_XTAL_GUARANTEED as u8;
```

或者在 `mpsl_3mode_central.rs` 中也添加 HFCLK 请求：
```rust
// 类似 mpsl_3mode_event.rs 的做法
spawner.spawn(hfclk_task(mpsl).unwrap());
```

#### 改动 3：添加 NVIC::unpend(RADIO)

如果确认需要保留 NVIC::unmask（方案 A 验证后决定），在 RADIO init 之后添加：

```rust
cortex_m::peripheral::NVIC::unpend(pac::Interrupt::RADIO);
unsafe {
    cortex_m::peripheral::NVIC::unmask(pac::Interrupt::RADIO);
}
```

### 方案 B：架构对齐（如果方案 A 不够）

参照 Nordic 官方模式重构为三层优先级：

1. `SIGNAL_START`：只做 TIMER0 配置 + RADIO power on → 触发软件中断
2. 软件中断 handler（priority 1）：做完整 ESB init + start RX/TX
3. `SIGNAL_RADIO`：调用 ESB RADIO event handler

这需要在 embassy 中选择一个未使用的 IRQ（如 `LPCOMP_IRQn`、`QDEC_IRQn`）作为软件触发中断，用 `NVIC_SetPendingIRQ` 在 timeslot callback 中触发。

## 6. 验证步骤

1. 先只做**改动 1**（移除 NVIC::unmask），测试 BLE 未连接时是否仍正常
2. 加上**改动 2**（HFCLK XTAL_GUARANTEED），测试 BLE 连接后是否 rx > 0
3. 如果仍有问题，加**改动 3**（NVIC::unpend）
4. 观察日志中 `radio` 和 `rx` 计数器变化
5. 如果以上不够，考虑方案 B 架构重构

## 7. 参考来源

- [Nordic Blog: Running micro-ESB concurrently with BLE](https://devzone.nordicsemi.com/nordic/nordic-blog/b/blog/posts/running-micro-esb-concurrently-with-ble) — 官方 ESB+BLE timeslot 完整模式
- [MPSL Timeslot Header](https://github.com/nrfconnect/sdk-nrfxlib/blob/master/mpsl/include/mpsl_timeslot.h) — HFCLK 警告、SIGNAL 定义
- [DevZone: SIGNAL_RADIO expected usage](https://devzone.nordicsemi.com/f/nordic-q-a/91837) — "不要自己 NVIC_EnableIRQ(RADIO_IRQn)"
- [NCS ESB driver (Zephyr)](https://github.com/nrfconnect/sdk-nrf/blob/main/subsys/esb/esb.c) — `CONFIG_ESB_MPSL_TIMESLOT` 官方实现
- [nRF5 SDK nrf_esb.c](https://github.com/particle-iot/nrf5_sdk/blob/master/components/proprietary_rf/esb/nrf_esb.c) — 官方 ESB PRX 状态机
- [inductivekickback/timeslot](https://github.com/inductivekickback/timeslot) — Rust MPSL timeslot wrapper
- [DevZone: ESB and BLE in NCS](https://devzone.nordicsemi.com/f/nordic-q-a/101592/esb-and-ble-in-nrf-connect-sdk-an-impossible-pair) — 官方承认 ESB+BLE 支持有限
- [MPSL Timeslot Guide (NCS)](https://devzone.nordicsemi.com/guides/nrf-connect-sdk-guides/b/software/posts/updating-to-the-mpsl-timeslot-interface) — NCS 下的 timeslot 配置指南
