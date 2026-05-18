# M10: MPSL Timeslot Adapter — 详细开发与验证计划

> 风格沿用 `m9-verification.md`：逐步骤、每步独立可验证、明确通过/失败标准。
> 上游 `docs/plan.md` 的 M10 章节给出概念蓝图；本文是其落地执行版。
> 本文已根据真实应用场景（多分体键盘多模 + 通用开源库定位）扩展了原 plan.md 中刻意回避的部分。

---

## 0. 项目背景与 M10 范围决策

**目标应用场景**：
- **多分体键盘**：副手 PTX → 主手 PRX（ESB），主手同时通过 BLE 连 PC。主手 ESB+BLE 在同一颗 nRF52 上共存。
- **Dongle 模式**：dongle 作 PRX，multi-pipe 同时连多个 PTX。dongle 独占模式（无 BLE）。

**库定位**：通用 ESB 库（非 RMK 专属），需向社区贡献。

**M10 范围决策（讨论结论）**：

| 项目 | 决定 | 依据 |
|------|------|------|
| 角色覆盖 | **PTX + PRX 双向**都支持 timeslot | 主手 PRX 必须在 timeslot 内监听副手 |
| 同步责任 | **异步路径 + observable hints** | thin abstraction 风格；和 Embassy / `embassy-net` 一致；RMK split 层已有事件确认重试逻辑 |
| BLE 共存验证 | **nrf-sdc 真实广告 + 连接** | 通用库必须自证 BLE 端到端可工 |
| API 形态 | `EsbTimeslot::wrap(esb, mpsl)` wrapper | 独占路径零回归；wrap 模式让 timeslot 与独占都是一等公民 |
| 调度策略 | 库支持 `Chained` / `Periodic` / `Manual` 三种 | 通用库不假定单一应用偏好 |
| 多 pipe | 独占 + timeslot 都支持 | dongle 与主手两种场景均需要 |
| 副手 | 独占 PTX，无 BLE | 简化系统，零 M10 代码改动 |

**显式不在 M10 范围**：
- LFCLK 级别时间同步（副手对齐主手 timeslot 节奏）
- 副手 BLE 共存（副手永远独占 ESB）

---

## 1. 关键参考（已审 + 新增）

| 来源 | 类型 | 用途 |
|------|------|------|
| `~/.cargo/registry/.../nrf-mpsl-0.3.0/src/flash.rs` | **Rust 参考实现** | 唯一在 Rust 中已工作的 timeslot 用例；STATE / `Timer0RawMutex` / 回调骨架 / BLOCKED 恢复策略 直接照抄 |
| `~/.cargo/registry/.../nrf-mpsl-0.3.0/src/mpsl.rs` | API 定义 | `with_timeslots` / `SessionMem<N>` / `HighPrioInterruptHandler`(RADIO/TIMER0/RTC0) |
| `~/.cargo/registry/.../nrf-mpsl-sys-0.2.1/.../mpsl_timeslot.h` | C 头文件 | 信号常量 / `mpsl_timeslot_request_t` / `signal_return_param_t` 真实结构 |
| [Nordic MPSL Timeslot Guide](https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/mpsl/doc/timeslot.html) | 官方文档 | 信号状态机 / 扩展机制 / 优先级/HFCLK 选项 |
| [Nordic DevZone: Updating to MPSL Timeslot Interface](https://devzone.nordicsemi.com/) | 官方教程 | BLOCKED/CANCELLED 恢复的标准模式 |
| [`alexmoon/nrf-sdc`](https://github.com/alexmoon/nrf-sdc) | Rust BLE 协议栈 | Step 7 BLE 共存验证；与 nrf-mpsl 同 repo 同维护者，最稳 |
| [`too1/ncs-esb-ble-mpsl-demo`](https://github.com/too1/ncs-esb-ble-mpsl-demo) | C 参考 | RADIO POWER cycle + 手动 ESB suspend/resume 序列 |
| [`inductivekickback/ncs_ble_esb_demo`](https://github.com/inductivekickback/ncs_ble_esb_demo) | C 参考 | PID 持久化模式 |
| [`nrfconnect/sdk-nrf/samples/esb/`](https://github.com/nrfconnect/sdk-nrf/tree/main/samples/esb) | Nordic 官方 | `esb_ptx_ble` / `esb_prx_ble` 整体架构（PTX 与 PRX 双向都有官方样本） |
| [Zephyr ESB driver](https://github.com/zephyrproject-rtos/zephyr/tree/main/subsys/esb) | C 实现 | `CONFIG_ESB_NEVER_DISABLE_TX=n` + timeslot integration 的工业级实现 |

---

## 2. 核心架构发现（源码验证）

> 这些是我读 `nrf-mpsl-sys` / `nrf-mpsl` 源码后的结论，取代 plan.md 中"待回答问题"。

1. **`nrf-mpsl` 0.3 不暴露 timeslot 高级 API。** 只提供 `MultiprotocolServiceLayer::with_timeslots()` 设置 session 数和 `SessionMem<N>` 提供静态内存。应用必须直接调 `raw::mpsl_timeslot_session_open / _request / _close`，自己写 `unsafe extern "C"` 回调。`flash.rs` 是 in-tree 范本。

2. **回调在 NVIC P0 上下文执行**（与 `MPSL_IRQ_TIMER0_Handler` 同优先级，等价 ZLI）。不能调用 Embassy `Signal::signal()`（含 `CriticalSectionRawMutex`）；可以调用 `WakerRegistration::wake()` / `AtomicWaker::wake()`（无锁）。`flash.rs` 已采用此模式。

3. **RADIO IRQ 路由问题已解决**：MPSL 把 timeslot 期间的 RADIO IRQ 作为 `signal=2 (MPSL_TIMESLOT_SIGNAL_RADIO)` 投递给回调。**不需要自定义 RADIO IRQ router**。

4. **TIMER0 同样被路由为 `MPSL_TIMESLOT_SIGNAL_TIMER0`**。ESB 协议定时仍走 TIMER1/2（已设计）。

5. **同步模式**：全局 `static STATE`，内层 `Mutex<Timer0RawMutex, RefCell<InnerState>>`。`Timer0RawMutex` 只屏蔽 TIMER0 NVIC bit，不破坏其他 MPSL 时序。

6. **资源占用**（`nrf-mpsl-0.3.0/src/mpsl.rs::Peripherals` 字段定义）：MPSL 在 nrf52 占用 RTC0 / TIMER0 / TEMP / PPI_CH19 / PPI_CH30 / PPI_CH31。**ESB 默认用 TIMER1 + PPI 避开 19/30/31**——`src/timer.rs` 已经是 TIMER1。

7. **`critical-section` 必须用 nrf-mpsl 自己的实现**：`nrf-mpsl/critical-section` feature 启用后注册自身实现（仅屏蔽部分 IRQ，保留 P0）。与 `cortex-m/critical-section-single-core` 冲突——**mpsl example 必须用 `[features]` 把 cs-single-core 隔离掉**。

8. **`EsbIsr::on_radio_interrupt()` / `on_timer_interrupt()` 已是普通方法**（不是 IRQ handler 内嵌逻辑）。timeslot 回调里可以直接调同一份方法。**T3 ISR 重构成本远小于原估**。

9. **现有 `EsbIsr` 用 Embassy `Signal::signal()` 发 suspend 完成信号**（`src/isr.rs:155`），这个对 P0 调用不安全。**timeslot 模式必须改为 `AtomicWaker`**——这是 M10 主要的 ESB 核心改动。

10. **PRX-in-timeslot 的特殊性**（plan.md 标为 high risk，本计划必做）：PRX 模式需要在 timeslot 开始后立刻 `RXEN` 并保持监听到 slot 结束。副手 PTX 不知道主手何时在听，**靠 ESB 重传 + 上层重试兜底**（异步路径）。

---

## 3. API 设计（草案）

### 3.1 整体形态

```rust
// 独占模式（M9 已就绪，零改动）
let esb = EsbPtx::new(radio, timer1, config);
esb.send(&payload).await?;

// Timeslot 模式（M10 新增）
let mpsl = MultiprotocolServiceLayer::with_timeslots::<_, _, 1>(...);
let esb = EsbPtx::new(radio, timer1, config);     // 同一个 EsbPtx
let mut esb_ts = EsbTimeslot::wrap(esb, &mpsl, TimeslotConfig {
    slot_length_us: 6000,
    schedule: ScheduleMode::Chained,
    hfclk: HfclkCfg::XtalGuaranteed,
    priority: TimeslotPriority::Normal,
});
esb_ts.start()?;
esb_ts.send(&payload).await?;       // 透明跨越 timeslot 边界

// Observable hints（可选用）
if esb_ts.slot_active() { /* ... */ }
esb_ts.slot_started().wait().await;
esb_ts.slot_ended().wait().await;
```

### 3.2 调度策略

```rust
pub enum ScheduleMode {
    /// 链式：SIGNAL_TIMER0 → ACTION_REQUEST 续下一 slot。最大 ESB 吞吐，BLE 靠 HIGH 优先级抢。
    /// 适用：主手 PRX，ESB 流量为主，BLE 偶发。
    Chained,

    /// 周期性：app task 定时申请。BLE 拿大头，ESB 平均延迟 = interval/2。
    /// 适用：BLE 流量为主，ESB 间歇。
    Periodic { interval_us: u32 },

    /// 手动：仅当 app 调 `request_slot()` 时申请。
    /// 适用：完全应用驱动的场景。
    Manual,
}
```

### 3.3 PRX-in-timeslot 接口

```rust
pub struct EsbTimeslotPrx<'d, ...> { ... }

impl<'d, ...> EsbTimeslotPrx<'d, ...> {
    pub fn wrap(prx: EsbPrx, mpsl: &'d MultiprotocolServiceLayer<'d>, cfg: TimeslotConfig) -> Self;
    pub async fn receive(&mut self) -> Result<ReceivedPacket, Error>;  // 跨 timeslot 透明
    pub fn send_ack_payload(&mut self, pipe: u8, payload: &[u8]) -> Result<(), Error>;
    pub fn slot_active(&self) -> bool;
    pub fn slot_started(&self) -> &Signal<NoopRawMutex, ()>;
    pub fn slot_ended(&self) -> &Signal<NoopRawMutex, ()>;
}
```

**关键语义**：`receive()` 在 slot 关闭期间挂起；slot 开窗后恢复监听；收到包立即唤醒。对调用方完全透明。

### 3.4 Wrap/Unwrap 可逆生命周期

```rust
// wrap：独占 → timeslot 模式
let esb_ts = EsbTimeslot::wrap(esb, &mpsl, cfg);

// unwrap：timeslot → 独占模式（三模热切换场景）
let esb = esb_ts.unwrap();  // session_close + RADIO 恢复 → 返还原始 EsbPtx/EsbPrx
esb.send(&payload).await?;  // 回到独占模式，零开销
```

**设计约束**：
- `unwrap()` 必须等当前 timeslot 结束（如有活跃 slot），执行 `session_close`，恢复 RADIO 到独占状态。
- `unwrap()` 后返还的 `EsbPtx`/`EsbPrx` 行为与 wrap 前完全一致——PID、地址配置、pipe 状态均保留。
- wrap/unwrap 可多次调用（BLE↔USB 热切换场景），无资源泄漏。
- `EsbTimeslot` 采用 move 语义持有 `EsbPtx`/`EsbPrx`，wrap 后原始实例不可用，unwrap 后 `EsbTimeslot` 不可用——编译期保证同一时刻只有一种模式活跃。

### 3.4 Observable Hints

参考 [`embassy-net::Stack::is_link_up()`](https://docs.embassy.dev/embassy-net/git/default/struct.Stack.html#method.is_link_up) 的设计哲学：库暴露状态，不强加 policy。

- `slot_active() -> bool` — 当前是否在 RX 窗口（同步查询）
- `slot_started: Signal<NoopRawMutex, ()>` — 窗口刚开（异步边界事件）
- `slot_ended: Signal<NoopRawMutex, ()>` — 窗口要关（异步边界事件）

> 注：这里用 `NoopRawMutex` 而非 `CriticalSectionRawMutex`，因为 Signal 只在 app task 单线程消费；写入只发生在 timeslot 回调 P0 唯一上下文（用 atomic 写边沿）。具体实现可能改用 `AtomicWaker` + 内部 atomic flag，对外封装成 Signal-like API——实现时根据 P0 安全性最终决定。

---

## 4. 模块分层

```
src/
├── (M1–M9 已完成的核心模块)
├── suspend.rs                 ✓ 已存在
├── isr.rs                     ← 需要小改：Signal → AtomicWaker（mpsl feature gated）
├── state_machine.rs           ✓ 已有 handle_radio_event
└── mpsl_timeslot.rs           ← M10 新增 ~600 行
                                  - struct EsbTimeslot<'d, ...>（PTX 包装）
                                  - struct EsbTimeslotPrx<'d, ...>（PRX 包装）
                                  - 全局 static STATE（仿 flash.rs）
                                  - unsafe extern "C" fn timeslot_session_callback
                                  - ScheduleMode 实现
                                  - Observable hints 内部状态

examples/
├── (M1–M9 已有的)
├── mpsl_smoke.rs              ← Step 0：MPSL init 冒烟（无 timeslot）
├── mpsl_request_basic.rs      ← Step 1：单次 timeslot 申请
├── mpsl_request_chained.rs    ← Step 2：链式申请 + 阻塞恢复
├── mpsl_ptx_in_slot.rs        ← Step 3-4：PTX 跑在 timeslot 内
├── mpsl_prx_in_slot.rs        ← Step 5-6：PRX 跑在 timeslot 内（multi-pipe + ACK）
├── mpsl_prx_ble.rs            ← Step 7：PRX + nrf-sdc BLE 真实共存
└── mpsl_split_e2e.rs          ← Step 8：副手独占 PTX → 主手 PRX-in-slot + BLE
```

---

## 5. 验证步骤

> 每步：目标 / 做什么 / 参考来源 / 通过标准 / 失败排查。前序通过才进下一步。

### Step 0: MPSL 初始化冒烟（0.5 天）

**目标**：MPSL init 跑通，`build_revision()` 能读，温度合理。**无 timeslot，无 ESB**。

**参考**：
- `nrf-mpsl-0.3.0/src/lib.rs` 顶部 doc example
- [Nordic MPSL Init Sequence](https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/mpsl/doc/api.html#mpsl_init)

**做什么**：
1. `Cargo.toml`：解决 `critical-section` 冲突（example 用 `[features]` 排除 cs-single-core）。
2. `examples/mpsl_smoke.rs`：
   - `bind_interrupts!` 绑定 `SWI0_EGU0 / POWER_CLOCK / RADIO / TIMER0 / RTC0`。
   - `MultiprotocolServiceLayer::new(...)`（**不带** timeslots 先验）。
   - spawn `mpsl.run()`。
   - main loop 每 5s 打印 `build_revision()` + `get_temperature()`。

**测试验证**：
- defmt RTT 输出 `MPSL build revision: <16 字节>`，温度 20–35 °C。
- **稳定性**：连续跑 30 分钟，无 `assert_handler` 触发，无 panic。
- **回归保护**：所有 M9 examples 不带 `mpsl` feature 仍能编译通过（`cargo build --example ptx_basic --features nrf52840,defmt`）。

**失败排查**：
1. 链接错误 `multiple `_critical_section_*` defined`：cs-single-core 冲突，调整 example features。
2. assert_handler 触发：通常是 LFCLK 配置错误，对照 `nrf-mpsl` doc example 的 `mpsl_clock_lfclk_cfg_t`。
3. RTT 没输出但板子在跑：可能 MPSL 把 SWI0_EGU0 中断的 priority 改了，影响 defmt-rtt——把 `defmt-rtt` 改用 SWI 之外的通道（已是默认）。

---

### Step 1: 单次 timeslot 申请 + 回调日志（1 天）

**目标**：开 session，请求 1 个 5 ms EARLIEST timeslot，看到 `SIGNAL_START` → `SIGNAL_TIMER0` → `SIGNAL_SESSION_IDLE` 序列。**无 ESB**。

**参考**：
- `nrf-mpsl-0.3.0/src/flash.rs:192-255` (`do_op`)
- `nrf-mpsl-0.3.0/src/flash.rs:302-361` (`timeslot_session_callback`)
- [`mpsl_timeslot.h`](https://github.com/nrfconnect/sdk-nrfxlib/blob/main/mpsl/include/mpsl_timeslot.h)（信号常量定义）

**做什么**：
1. 切换到 `with_timeslots::<_, _, 1>` + `static SESSION_MEM: StaticCell<SessionMem<1>>`。
2. `src/mpsl_timeslot.rs` 骨架：
   - `static STATE: State`（仿 `flash.rs:151`）
   - `InnerState`：`session_id`、`return_param`、`request`、信号计数器（start/timer0/radio/blocked/cancelled/idle）
   - `unsafe extern "C" fn cb(session_id, signal)`：
     - `SIGNAL_START`：设 TIMER0 CC[0]=4500us，`ACTION_NONE`
     - `SIGNAL_TIMER0`：`ACTION_END`
     - `SIGNAL_SESSION_IDLE`：`state.waker.wake()`，返回 null
     - 其它信号计数后返回 null
3. `examples/mpsl_request_basic.rs`：open session → request 5ms NORMAL EARLIEST → `poll_fn` 等 idle → 打印计数。

**测试验证**：
- 输出 `start=1 timer0=1 radio=0 idle=1 blocked=0 cancelled=0`。
- 用逻辑分析仪/示波器测 P1.xx GPIO（在 SIGNAL_START 入口设 1、END 处清 0）：脉冲宽度 ≈ 5 ms ± 100 µs。
- **无 `SIGNAL_OVERSTAYED`**（assert 抑制时通过；启用 panic-on-overstayed 时不触发）。
- **逻辑断言**：连续运行 100 次单次申请，每次都收到完整序列，0 失败。

**失败排查**：
1. `session_open` 返回非 0：`with_timeslots::<N>` 的 N ≥ 1；`SessionMem` 是 `&'static mut`。
2. 永远没回调：检查 `bind_interrupts!` 包含 RADIO/TIMER0/RTC0；他们的优先级仍是 P0（不被覆盖）。
3. `SIGNAL_BLOCKED` 立即返回：超时设 1 000 000 µs；EARLIEST 先 NORMAL，再次 BLOCKED 升 HIGH。

---

### Step 2: 链式申请 + BLOCKED/CANCELLED 恢复（1 天）

**目标**：100 个 5 ms timeslot 串成 ~500 ms 连续流；BLOCKED 自动升级恢复。

**参考**：
- `nrf-mpsl-0.3.0/src/flash.rs:316-336`（`ACTION_REQUEST` 在 `SIGNAL_TIMER0` 分支续 slot）
- `nrf-mpsl-0.3.0/src/flash.rs:347-355`（BLOCKED/CANCELLED 升 HIGH 重试）
- [`mpsl_timeslot.h` BLOCKED handling section](https://github.com/nrfconnect/sdk-nrfxlib/blob/main/mpsl/include/mpsl_timeslot.h)

**做什么**：
1. 在 `SIGNAL_TIMER0` 分支：`return_param.callback_action = ACTION_REQUEST`，`return_param.params.request.p_next = &state.request`。
2. 主 task 等 `start_count == 100` 后 `session_close`。
3. `SIGNAL_BLOCKED / CANCELLED`：升 priority 到 HIGH、timeout 改 `EARLIEST_TIMEOUT_MAX_US`，调 `mpsl_timeslot_request`（不经 return_param）。
4. defmt 每 10 个 timeslot 打一次计数。

**测试验证**：
- 100 个 timeslot 全完成，总耗时 500–600 ms（≤20% jitter）。
- **故意触发 BLOCKED**：测试中临时把第一次请求设极高 priority + 极短 timeout（1µs），观察 `blocked_count >= 1` 后自动恢复，最终 `start_count == 100`。
- **故意触发 CANCELLED**：在中途 `session_close` 一次再重开，验证 cancelled path（暂可推到后续 step）。

**失败排查**：
1. 第 2 个 slot 没来：`return_param.params.request.p_next` 必须指向 `STATE` 内部稳定地址，不能是栈上临时值。
2. BLOCKED 后死循环：HIGH 优先级仍 BLOCKED → LFCLK 配置错误 / 优先级与 MPSL 内部活动冲突。

---

### Step 3: PTX-in-timeslot 单包（1.5 天）

**目标**：每个 timeslot 内做 RADIO POWER cycle + ESB init + 发 1 个 ACK 包 + ESB disable。slot 6 ms。

**参考**：
- [`too1/ncs-esb-ble-mpsl-demo/.../timeslot_handler.c`](https://github.com/too1/ncs-esb-ble-mpsl-demo/blob/main/src/timeslot_handler.c)（SIGNAL_START 处的 RADIO POWER cycle + ESB init 顺序，必读）
- [`inductivekickback/ncs_ble_esb_demo/.../timeslot.c`](https://github.com/inductivekickback/ncs_ble_esb_demo/blob/main/src/timeslot.c)（PID save/restore 位置）
- 当前仓库 `src/radio.rs::init()` + `src/state_machine.rs`
- 当前仓库 `src/isr.rs::on_radio_interrupt`（直接复用）

**做什么**：
1. `src/isr.rs` 改造（mpsl feature gated）：把 `Signal::signal()` 改为 `AtomicWaker::wake()`。
2. `mpsl_timeslot.rs` 增加 `EsbTimeslot<'d, ...>` PTX 包装。
3. `SIGNAL_START` 回调流程（**严格按 too1 C 顺序**）：
   - `NRF_RADIO->POWER = 0; NRF_RADIO->POWER = 1;`
   - 调 ESB register init（复用 `radio.rs::init()` 内核逻辑）
   - 恢复 PID（`STATE.saved_pid`）
   - 触发 1 次 TX
4. `SIGNAL_RADIO` 分支：调 `EsbIsr::on_radio_interrupt()`（已有方法）。
5. `SIGNAL_TIMER0` 分支：保存 PID → ESB cleanup → `ACTION_END`。
6. `mpsl_ptx_in_slot.rs` example：每 30 ms 申请一次 6ms timeslot，发计数器递增 payload；对侧用 M9 `prx_usb.rs`（独占）验收。

**测试验证**：
- **基础**：1 分钟（~2000 timeslot）PRX 端 USB CDC 显示 ≥ 1900 包计数器单调递增。
- **0 OVERSTAYED**：assert 启用情况下不触发。
- **0 duplicate**：PRX 不报 PID 重复（验证 PID save/restore 正确）。
- **GPIO 时序断言**（逻辑分析仪/示波器）：SIGNAL_START 到第一次 TX disabled 事件 ≤ 200 µs（init 开销可控）。
- **回归**：M9 `ptx_basic` / `prx_usb` 不带 mpsl feature 仍 100% 通过（运行 5 分钟流测试）。

**失败排查**：
1. PRX 完全收不到：在 `SIGNAL_START` 临时加 defmt（**仅调试**），确认 ESB init 跑完且 `RADIO->FREQUENCY` 写对。
2. CRC 错：RADIO POWER cycle 后某些寄存器需重写（`PCNF1` / `SHORTS`）——对照 `radio.rs::init()` 全量复跑。
3. duplicate：PID save 时机错位——确认在 `SIGNAL_TIMER0` 而非 `ACTION_END` 之后做。
4. OVERSTAYED：transaction 超时——减重传到 1 / 缩 ACK timeout / 增 slot 到 7 ms。

---

### Step 4: PTX PID 持久化跨 timeslot（1 天）

**目标**：1000 包跨 ≥ 100 timeslot 边界，PRX 0 duplicate。

**参考**：
- [`inductivekickback/ncs_ble_esb_demo`](https://github.com/inductivekickback/ncs_ble_esb_demo) PID 模式
- ESB 协议规范：[Nordic ESB 文档](https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/esb/doc/esb_users_guide.html)

**做什么**：
1. PTX：每 timeslot 内连发 10 包（6ms slot ≈ 9 个 TX+ACK 周期可行），跑 100 timeslot。
2. PID save/restore 已在 Step 3 实现，本步骤是验证。
3. PRX 用 M9 `prx_usb.rs`，统计 `duplicates` 字段。

**测试验证**：
- 1000 包接收 ≥ 990，**duplicates == 0**。
- defmt 在 slot 边界一次性 dump PID：slot N 最后一包 PID = X，slot N+1 第一包 PID = (X+1) mod 4。
- **极端测试**：把 slot 缩到 3 ms（每 slot 只发 ~3 包），跑 1000 包，duplicates 仍 == 0。

---

### Step 5: PRX-in-timeslot 基础（1.5 天）

**目标**：主手 PRX 在 timeslot 窗口内可靠收包。副手用独占 PTX。

**参考**：
- [`nrfconnect/sdk-nrf/samples/esb/esb_prx_ble`](https://github.com/nrfconnect/sdk-nrf/tree/main/samples/esb)（Nordic 官方 PRX+BLE 样本，必读）
- [Zephyr ESB `subsys/esb` timeslot integration](https://github.com/zephyrproject-rtos/zephyr/blob/main/subsys/esb/esb.c)（PRX 在 timeslot 内 RXEN 的序列）

**做什么**：
1. `EsbTimeslotPrx<'d, ...>` 包装（与 PTX 对偶）。
2. `SIGNAL_START`：RADIO POWER cycle → ESB PRX init → 立即 `tasks_rxen = 1`。
3. `SIGNAL_RADIO`：调 PRX 的 `on_radio_interrupt`（注：需要在 `EsbIsr` PRX 分支同样存在，确认 `src/isr.rs` PRX 路径已有）。
4. `SIGNAL_TIMER0`：disable RX → 清 RADIO → `ACTION_END`。
5. **Observable hints 实现**：`slot_started.signal()` 在 SIGNAL_START 末尾；`slot_ended.signal()` 在 SIGNAL_TIMER0 入口。这两个 signal 用 atomic flag + waker（不用 Embassy Signal，避免 P0 critical section）。
6. `mpsl_prx_in_slot.rs` example：PRX 在 timeslot 内监听，每收一包通过 USB CDC 报告 + 包内计数器。对侧用 M9 `ptx_silent.rs`（独占）每 10 ms 发一包。

**测试验证（功能验证层——见 §5.1 两层指标）**：
- **基础接收**：60 秒（6000 个副手包）主手收到 ≥ 4000（67%）。注：这个数字反映 timeslot 占空比（slot 6ms / 周期 10ms = 60% 窗口），不是丢包率。
- **窗口内丢包率**：只统计 slot_active 时段内的包，丢包率 < 1%（与独占模式相当）。
- **窗口外丢包**：BLE 段（slot 外）副手发的包，PRX 显式不接收（验证 slot 关闭后 RADIO 确实 disable）。
- **Observable hints 校验**：
  - `slot_active()` 在 SIGNAL_START 与 SIGNAL_TIMER0 之间返回 true。
  - `slot_started/slot_ended` Signal 边沿与 GPIO 探针同步。
- **回归**：M9 `prx_usb` 独占模式 5 分钟流测仍 100% 通过。

> 键盘场景指标（端到端 0 按键丢失、< 15ms 延迟）在 Step 8 验证。

**失败排查**：
1. 主手完全收不到：`SIGNAL_START` 内 `tasks_rxen = 1` 没触发——逻辑分析仪测 RADIO STATE 寄存器；可能 RADIO POWER cycle 后 base/prefix 地址寄存器需重写。
2. slot_active 与实际窗口不一致：边沿写入时机错——确认在 `STATE.with_inner` 内做。
3. 窗口外收到包：RADIO 没关干净——TIMER0 信号处理时确保 `tasks_disable = 1` 且等到 DISABLED 事件。

---

### Step 6: PRX multi-pipe + ACK payload（1 天）

**目标**：主手 PRX 在 timeslot 内同时收 2 个 pipe 的包，且能发回 ACK payload。

**参考**：
- 当前仓库 `src/addresses.rs`（多 pipe 地址配置）
- M9 Step 6 的 `ptx_multipipe.rs`（独占模式 multi-pipe 已验证）
- M9 Step 3 的 ACK payload 验证

**做什么**：
1. 配置 2 个 pipe（独立 prefix）。
2. 两侧（模拟两个副手）用独占 PTX 在不同 pipe 发包，交替触发。
3. 主手 PRX 在 timeslot 内：每收一包通过 `send_ack_payload(pipe, &[counter])` 回 ACK payload。
4. 副手验证 ACK payload 收到。

**测试验证**：
- 两个副手各发 500 包，主手收到 ≥ 700 / 1000 总数，且 pipe 号 100% 正确。
- 每个副手收到 ACK payload ≥ 700 / 500 自己发的包（注：副手按 timeslot 占空比也会丢一部分）。
- ACK payload 内容正确（counter 单调）。

---

### Step 7: BLE 共存（nrf-sdc 真实广告 + 连接）（2 天）

**目标**：主手同时跑 ESB PRX-in-timeslot + BLE 广告 + BLE 连接（手机 nRF Connect 连上）。

**参考**：
- [`alexmoon/nrf-sdc` examples](https://github.com/alexmoon/nrf-sdc/tree/main/examples)（与 nrf-mpsl 同 repo；`ble_central` / `ble_peripheral` 样本）
- [`nrf-sdc/nrf-sdc/src/peripheral.rs`](https://github.com/alexmoon/nrf-sdc/blob/main/nrf-sdc/src/peripheral.rs)（标准广告+连接 setup）
- `nrfconnect/sdk-nrf/samples/esb/esb_prx_ble`（C 参考，整体架构）

**做什么**：
1. dev-dependencies 加 `nrf-sdc = "0.1"`（与 nrf-mpsl 同 repo）+ `trouble-host`（BLE 协议层）。
2. `mpsl_prx_ble.rs` example：
   - 同一个 `MultiprotocolServiceLayer` 实例供 nrf-sdc 和 EsbTimeslotPrx 共享。
   - spawn nrf-sdc peripheral task（广告 + GATT echo service）。
   - spawn EsbTimeslotPrx task（接副手包，USB CDC 输出）。
3. 设 BLE connection interval 30 ms、slave latency 3。
4. ESB timeslot 用 `Chained` 模式 + Normal 优先级（BLE 用 HIGH 自动抢占）。

**测试验证（功能验证层——保守 BLE 参数 CI=30ms/SL=3，见 §5.1）**：
- 用 nRF Connect mobile app 扫到广告、连上、订阅 GATT echo 特征——**1 分钟稳定无断连**。
- **GATT echo 往返延迟** < 100 ms（功能验证门槛，非键盘目标）。
- **ESB 接收率**：与 Step 5 / 6 相比下降 ≤ 30%（绝对值 ≥ 50% 副手发出量；剩余靠副手重传补齐）。
- **0 panic、0 OVERSTAYED、0 assert**：连续跑 30 分钟。

> 键盘场景 BLE 参数（CI=7.5ms/SL=0）和延迟指标在 Step 8 切换验证。

**失败排查**：
1. BLE 广告不可见：检查 nrf-sdc 与 nrf-mpsl 版本兼容（同 repo `Cargo.toml` workspace 看一致版本）。
2. ESB 完全失效：BLE HIGH 优先级抢满 → 调 BLE conn interval 长一点（100 ms）。
3. assert in MPSL：可能是 nrf-sdc 和 EsbTimeslot 都试图 `with_timeslots`——必须共享同一个 MPSL 实例。

---

### Step 8: 端到端分体场景（1.5 天）

**目标**：复现真实键盘使用——副手独占 PTX → 主手 timeslot PRX + BLE 连接 PC。**切换到键盘级 BLE 参数，验证 §5.1 键盘场景指标。**

**做什么**：
1. **副手**：M9 `ptx_silent.rs`，每 10 ms 发计数器递增 payload（独占 ESB，无 BLE）。
2. **主手**：Step 7 的 `mpsl_prx_ble.rs`，**BLE 参数切换为 CI=7.5ms / SL=0**，PRX 输出经 USB CDC + GATT notify 双通道转发。
3. **PC**：USB CDC 监控丢包率 + nRF Connect 监控 GATT notify + 延迟测量。

**测试验证——功能验证层**：
- 60 秒（6000 副手包）：
  - 主手通过 USB CDC 收到 ≥ 4000（占空比基线）。
  - GATT notify 接收 ≥ 90% 主手已收的包（BLE 通路也工作）。
- 主动断开 BLE 连接：ESB 接收率应**回升到接近 Step 5/6 水平**（BLE idle 占用变小）。
- 主动重连 BLE：ESB 接收率回到 Step 7 水平。

**测试验证——键盘场景层（§5.1 指标，须优于全 BLE 基线）**：
- **端到端 0 按键丢失**：副手以 10ms 间隔发 1000 个递增序号包（模拟按键），主手端到端交付后序号无缺失（ESB 重传 + 应用层确认兜底）。
- **GATT 单程延迟 < 8 ms**：主手收到 ESB 包后打时间戳，GATT notify 到 PC 后打时间戳，CI=7.5ms 下平均 < 8ms。
- **GATT 往返延迟 < 16 ms**：PC 发 echo → 主手回 → PC 收到，CI=7.5ms 下平均 < 16ms。
- **ESB 副手→主手单程 < 2 ms**：GPIO 探针测副手 TX trigger → 主手 RX complete 间隔（全 BLE 基线 3.75ms avg，ESB 须明显优于此）。
- **端到端按键延迟 avg < 8 ms / worst < 12 ms**：副手按键扫描 → 主手 BLE notify 到达 PC（全 BLE 基线 avg ~8.5ms / worst ~16ms，ESB+BLE 方案须全面优于）。

---

### Step 9: 过夜长跑稳定性

**目标**：8 小时混合负载 0 panic、0 OVERSTAYED、无内存泄漏。

**做什么**：
- Step 8 配置不动跑 8 小时。
- 每 10 万包 defmt 一次累积统计（USB CDC log 到文件）。

**测试验证**：
- **端到端 0 按键丢失**（8 小时持续，应用层确认交付 100%）。
- 0 panic、0 OVERSTAYED、0 INVALID_RETURN。
- buffer pool 自由数量稳定（仿 M9 Step 8）。
- BLE 连接全程不断（或断了能 < 5 s 重连）。
- GATT notify 延迟 8 小时内无漂移（P99 < 15 ms，CI=7.5ms）。

---

## 5.1 验证指标分层：功能验证 vs 键盘场景

> Step 0–4 属基础设施验证，指标只需"能工作、不崩"。
> Step 5–9 涉及 PRX + BLE 共存，需区分**功能验证指标**（证明 adapter 正确）和**键盘场景指标**（证明满足应用需求）。

### 方案对比：ESB+BLE vs 全 BLE

| | 全 BLE（基线） | ESB+BLE（本方案） | 优势 |
|--|---------------|-------------------|------|
| 副手→主手平均延迟 | 3.75 ms (BLE CI=7.5ms) | **1–2 ms** (ESB) | 延迟砍半 |
| 副手→主手最坏延迟 | 7.5 ms | **~2 ms** + slot 等待 | 抖动更小 |
| 主手→PC 延迟 | 3.75 ms (BLE) | 3.75 ms (BLE) | 相同 |
| **端到端平均** | **~8.5 ms** | **~6–7 ms** | 快 ~2 ms |
| **端到端最坏** | **~16 ms** | **~11 ms** | 快 ~5 ms |
| 副手功耗 | 需维持 BLE CI 心跳 | ESB 仅有键时发包 | 显著更低 |
| 副手 Flash/RAM | BLE 协议栈 ~60KB/~20KB | 无需 BLE 协议栈 | 简化固件 |
| 主手 radio 调度 | 双 BLE 连接（Central+Peripheral） | ESB timeslot + 单 BLE | 更简单可控 |
| 重传延迟 | 等下一个 CI (7.5ms) | ESB auto-retransmit ~250µs | 快 30x |

> ESB+BLE 方案的核心价值：用 ESB 替代副手链路的 BLE，在延迟、功耗、固件复杂度三个维度同时优于全 BLE。
> 主手→PC 段仍用 BLE（兼容性需要），瓶颈在 BLE CI 下限 7.5ms，两个方案相同。

### 典型延迟参考

| 环节 | 全 BLE | ESB+BLE |
|------|--------|---------|
| 按键矩阵扫描 | ~1 ms | ~1 ms |
| 副手→主手 | 3.75 ms avg / 7.5 ms worst | **1–2 ms** avg / **~2 ms** worst |
| 主手→PC (BLE CI=7.5ms) | 3.75 ms avg / 7.5 ms worst | 3.75 ms avg / 7.5 ms worst |
| **端到端** | **~8.5 ms avg / ~16 ms worst** | **~6–7 ms avg / ~11 ms worst** |

人体感知阈值 ~30–50ms；< 10ms 端到端为优秀，< 15ms 良好，< 30ms 可接受。

### 两层指标定义

| 指标 | 功能验证（Step 5–7 当前定位） | 键盘场景（Step 8 必须达标） | 全 BLE 基线 |
|------|-------------------------------|----------------------------|------------|
| BLE conn interval | 30 ms（保守测共存） | **7.5 ms** | 7.5 ms |
| BLE slave latency | 3 | **0** | 0 |
| GATT 单程延迟 | < 50 ms | **< 8 ms** | < 8 ms |
| GATT 往返延迟 | < 100 ms | **< 16 ms** | < 16 ms |
| ESB 按键交付 | slot 内丢包 < 1%（射频层） | **端到端 0 按键丢失** | — |
| 副手→主手单程 | 不考核 | **< 2 ms**（须优于 BLE 的 3.75ms avg） | 3.75 ms avg |
| 端到端按键延迟 | 不考核 | **avg < 8 ms / worst < 12 ms**（须优于全 BLE 的 8.5/16ms） | avg ~8.5 ms / worst ~16 ms |

> **注**：slot 内丢包率是射频物理层指标，反映信道质量；键盘 0 丢失靠 ESB auto-retransmit + 应用层重试兜底，两者不矛盾。
> 占空比导致的"slot 外丢包"不是丢包——副手在主手 slot 外发的包本就不期望被收到，由副手重传机制补偿。

---

## 6. 实施顺序与工程改动

| 编号 | 任务 | 产物 | 估时 |
|------|------|------|------|
| T0 | Cargo cs 冲突方案 + Step 0 example | mpsl_smoke.rs 通过 | 0.5 天 |
| T1 | `src/mpsl_timeslot.rs` 骨架 + STATE + 回调 | Step 1 通过 | 1 天 |
| T2 | 链式 + BLOCKED 恢复 | Step 2 通过 | 1 天 |
| T3 | `src/isr.rs` Signal → AtomicWaker（mpsl gated） | M9 examples 独占回归通过 | 0.5 天 |
| T4 | PTX-in-timeslot + PID save/restore | Step 3-4 通过 | 2 天 |
| T5 | PRX-in-timeslot + Observable hints | Step 5 通过 | 1.5 天 |
| T6 | PRX multi-pipe + ACK payload | Step 6 通过 | 1 天 |
| T7 | nrf-sdc 接入 + BLE 共存 + 键盘场景指标 | Step 7-8 通过 | 2.5 天 |
| T8 | 过夜 + `docs/m10-verification.md` 收尾 | — | 1.5 天 |

**累计 11.5 天**。原 plan.md 估 5–7 天是范围只覆盖 PTX 单向的估算；本计划扩到 PRX 双向 + 真实 BLE 共存 + 键盘场景指标验证，对应**乐观 10 / 悲观 14 / 最可能 11–12 天**。

---

## 7. 不变量（写代码时反复核对）

1. `mpsl_timeslot.rs` 内所有跨上下文共享状态放 `STATE`，用 `with_inner` 访问。
2. `unsafe extern "C" fn callback` 内绝不调 `defmt`（生产）/ Embassy `Signal::signal` / alloc / panic 路径。
3. `return_param` 必须是 `STATE` 内成员，回调返其原始指针。栈上构造再返回 = UB。
4. `SIGNAL_OVERSTAYED` 必须 panic（同 flash.rs:357）。
5. ESB 拒绝把 TIMER0 / RTC0 / TEMP / PPI19/30/31 当可用资源。
6. `with_timeslots::<_, _, N>` 的 N ≥ 同时打开 session 数（我们只需 1）。
7. `Timer0RawMutex` 不替换 cortex-m 全局 critical-section——只是同步原语；cs 实现由 `nrf-mpsl/critical-section` feature 提供。
8. RADIO ISR 等价物（处理 SIGNAL_RADIO）可重入，无静态可变状态（除 STATE）。
9. Observable hints（slot_started / slot_ended / slot_active）只在 STATE.with_inner 内更新。
10. 独占模式编译路径必须与 `mpsl` feature 完全解耦——`#[cfg(not(feature = "mpsl"))]` 守护所有 mpsl-only 代码；M9 examples 不带 mpsl feature 仍 100% 通过。
11. `EsbTimeslot` wrap/unwrap 必须可逆——`unwrap()` 后返还的 `EsbPtx`/`EsbPrx` 状态（PID、地址、pipe 配置）与 wrap 前一致；多次 wrap/unwrap 循环无资源泄漏。这是三模热切换（BLE↔USB）的基础约束。

---

## 8. 风险栈（按发现重排，最大→最小）

| 风险 | 等级 | 缓解 |
|------|------|------|
| ESB ISR 改造（Signal→AtomicWaker）破坏 M9 独占模式 | 高 | T3 完成后立即回归 M9 step 4/7（5分钟+5分钟）；保留 cfg gate 让独占编译路径不引入 mpsl 代码 |
| MPSL critical-section 与 cs-single-core 链接冲突 | 高 | T0 验证；example 之间 `[features]` 隔离 |
| PRX-in-timeslot 在 slot 关闭瞬间漏收 | 中 | SIGNAL_TIMER0 中先 disable RX 再清理；GPIO 探针确认 RX 关闭边沿 |
| 回调 P0 内 ESB state machine 超时（OVERSTAYED） | 中 | TIMER0 CC[0] 设 slot 结束前 ≥ 500 µs；SIGNAL_TIMER0 抢救 |
| PID 持久化错位 | 中 | Step 4 显式 dump PID 边界值；duplicates 严格 == 0 |
| nrf-sdc + nrf-mpsl 版本不匹配 | 中 | 同 repo 同 workspace 版本；如有问题先 lockstep upgrade |
| 多 pipe 在 timeslot 内 RX 配置遗漏 | 中 | Step 6 显式覆盖；对照 M9 multi-pipe 寄存器 dump |
| Timer0RawMutex NVIC bit 偏移错 | 低 | 直接复用 flash.rs 实现 |
| BLE 连接事件抢占过严 | 低 | BLE 间隔 30–100 ms / slave latency 2–3 |

---

## 9. 与 plan.md 的差异

> 本文取代 plan.md M10 章节中"Open Questions"和 C4 fix（"PRX 独占简化假设"）。

| plan.md 中 | 本文结论 | 原因 |
|-----------|---------|------|
| nrf-mpsl 是否暴露 timeslot API？ | 否，用 `raw::*` + 仿 flash.rs | 源码验证 |
| Embassy Signal 替代 NVIC pending？ | `AtomicWaker` 可（无锁），`Signal` 不可（含 mutex） | 源码验证 |
| 最小 timeslot 多长？ | 5–6 ms，PTX 单包 / PRX 全程监听 | 估算 |
| 自定义 RADIO IRQ router？ | 不必要，`SIGNAL_RADIO` 已派发 | 源码验证 |
| C4: PRX 独占简化（dongle 不要 BLE） | 部分采纳：dongle/副手独占；**主手 PRX 必须 in timeslot** | 真实应用场景需要 |
| Step 5 BLE 共存可否推迟 | 否，必须 nrf-sdc 真实验证 | 通用库定位 |
| 同步责任 | 完全异步路径 + observable hints | Rust/Embassy/RMK 三角度推论 |

---

## 10. 验证完成产出

- `src/mpsl_timeslot.rs` — 实现（~600 行）
- `src/isr.rs` 小改 — `Signal → AtomicWaker`，mpsl feature gated
- `examples/mpsl_*.rs` — 7 个 example（Step 0–8）
- `docs/m10-verification.md` — 仿 m9-verification 的最终验证报告
- `Cargo.toml` — `mpsl` feature（已就绪） + `nrf-sdc` dev-dep
- README 增加 timeslot quickstart 段落（通用库推广用）

---

## 11. 回滚路径

如果 Step 5（PRX-in-timeslot）卡死：
- M10 deliverable 降级为 "PTX-in-timeslot + 文档化的 PRX 集成路径"——主手 PRX-in-timeslot 标 experimental，dongle/副手独占模式（M9）继续作为稳定路径。
- Step 7-8 BLE 共存只覆盖 PTX 方向（主手如果是 PTX 仍可用，比如 macro 板）。

如果 Step 7（nrf-sdc 接入）卡死：
- 降级为 Step 7' = "Flash::take() 并行 timeslot 验证"——证明 timeslot 调度多消费者下健康，BLE 验证写为 "TODO" 待 nrf-sdc 后续版本。

---

## 12. 下一步（实施前最后确认）

待你确认完毕后开始 T0：
- ① 调度策略 default：建议 `Chained`（键盘场景偏低延迟），是否同意？
- ② 回调内 ESB tick 边界：Step 3 先采用方案 A（回调内直接调 on_radio_interrupt），如果实测 > 500 µs 再 defer 到 SWI——是否同意？
- ③ Cargo cs 冲突解决方式：example 用 `required-features` + `[features]` 切换 cs-single-core，是否同意？
