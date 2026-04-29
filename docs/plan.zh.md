# nRF-ESB: 纯 Rust ESB + MPSL 时隙实现计划

## 背景

RMK 键盘项目目前使用 Nordic Gazell（专有预编译 C 库）实现 2.4GHz 无线通信。Gazell 阻碍上游合并（许可证问题）、无法通过 MPSL 与 BLE 共存、且角色锁定不可切换。目标是替换为纯 Rust ESB 实现：

1. **Phase 5A**：独占 RADIO 模式工作（直接替代 Gazell）
2. **Phase 7**：集成 MPSL 时隙，实现 BLE+ESB 并发
3. 独立的、可开源的 crate，提供 Embassy async API

参考实现是 `esb-ng`（jamesmunns/esb，fork 到 Raymond8196/esb，clone 到 `/home/qlg/wkspaces/esb-ng/`），提供了可用的 ESB 状态机，但架构不兼容（bbq2、nrf-pac 0.1、NVIC::pend、无 MPSL 感知）。

## 参考资料

### 代码参考

| 资源 | 位置 | 相关性 |
|------|------|--------|
| **esb-ng** (jamesmunns/esb) | `/home/qlg/wkspaces/esb-ng/` | RADIO 寄存器操作、PTX/PRX 状态机、定时常量的主要参考 |
| **NCS esb_ptx_ble** 示例 | `github.com/nrfconnect/sdk-nrf/samples/esb/esb_ptx_ble/` | Nordic 官方 ESB+BLE 并发示例 |
| **NCS esb_prx_ble** 示例 | `github.com/nrfconnect/sdk-nrf/samples/esb/esb_prx_ble/` | PRX 对应版本 |
| **too1/ncs-esb-ble-mpsl-demo** | `github.com/too1/ncs-esb-ble-mpsl-demo/` | 低层 MPSL 时隙处理程序，手动 ESB 挂起/恢复 |
| **inductivekickback/ncs_ble_esb_demo** | `github.com/inductivekickback/ncs_ble_esb_demo/` | Radio Notification 方法；PID 持久化模式；ZLI 解决方案 |
| **Nordic DevZone MPSL 指南** | `devzone.nordicsemi.com/.../updating-to-the-mpsl-timeslot-interface` | MPSL 时隙的概念框架 |
| **RMK Phase 4.1 PoC** | `examples/use_rust/nrf52840_radio_switch_poc/` | 已验证的动态 RADIO ISR 分派（AtomicU8 模式） |

### 官方文档

| 主题 | 来源 | URL |
|------|------|-----|
| **nRF52840 产品规格书** | Nordic PS v1.7 | https://docs.nordicsemi.com/bundle/ps_nrf52840/page/keydoc_html.html |
| **RADIO 外设** (PS §6.17) | Nordic PS | https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html |
| **TIMER 外设** (PS §6.24) | Nordic PS | https://docs.nordicsemi.com/bundle/ps_nrf52840/page/timer.html |
| **CRCCNF 寄存器** — SKIPADDR 字段 | Nordic PS §6.17.10 | SKIPADDR=1 时 CRC 不包含地址字段（ESB 标准；避免 errata [143]） |
| **PCNF0 寄存器** — S1INCL, LFLEN, S1LEN | Nordic PS §6.17.10 | S1INCL=Auto（复位默认值）：S1LEN>0 时 S1 包含在 RAM 中 |
| **PCNF1 寄存器** — MAXLEN, BALEN, ENDIAN | Nordic PS §6.17.10 | ESB 使用 ENDIAN=Big；MAXLEN=252；BALEN=4（5字节地址） |
| **EasyDMA / PACKETPTR** | Nordic PS §6.17.6 | DMA 指针必须字对齐；不能访问 RAM block 1（见 errata [122]） |
| **Errata [122]** EasyDMA RAM block 1 | Nordic Errata | https://docs.nordicsemi.com/bundle/errata_nrf52840_Rev3/page/ERR/nRF52840/Rev3/latest/err_840.html |
| **Errata [153]** RSSI 不准确 | Nordic Errata | https://docs.nordicsemi.com/bundle/errata_nrf52840_Rev3/page/ERR/nRF52840/Rev3/latest/anomaly_840_153.html |
| **Errata [204]** TX/RX 发射 | Nordic Errata | https://docs.nordicsemi.com/bundle/errata_nrf52840_Rev1/page/ERR/nRF52840/Rev1/latest/anomaly_840_204.html |
| **Nordic ESB 用户指南** (nRF5 SDK) | Nordic SDK Docs | https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html |
| **S1INCL 行为** | Nordic DevZone | https://devzone.nordicsemi.com/f/nordic-q-a/79717 |
| **MPSL Timeslot API** | nRF Connect SDK Docs | https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/mpsl/doc/timeslot.html |

## 专家评审发现

> 三份独立评审：Rust 专家、RMK 集成专家、ESB/BLE 协议专家。

### 严重问题（实现前必须修复）

**R1. `EsbTimer::regs()` — 无单例强制（Rust 专家）**
nrf-pac 0.3 暴露 `pub const TIMER1: Timer` — 任何人都可以直接访问硬件。trait 的 `regs()` 没有编译时保护。
**修复**：构造函数中接受 `embassy_nrf::peripherals::TIMER1`（单例），内部转为 PAC，仅通过 `EsbIrq` handle 的 `pub(crate)` 暴露。

**R2. `PacketPool` DMA use-after-free — 缺少在途状态（Rust 专家）**
三通道设计（`free`、`tx_queue`、`rx_queue`）使用 `usize` 索引，没有"在途"状态。当数据包指针交给 RADIO DMA 时，索引不在任何通道中。
**修复**：采用 esb-ng 的 grant 传递模式 — 在 `EsbRadio` 结构体中持有 `Option<PayloadR>`/`Option<PayloadW>` 直到 DMA 完成。或添加每个槽位的 `AtomicU8` 状态（free/queued/in_dma/rx_done）。

**R3. `MaybeUninit` 不适合 DMA 缓冲 — 无对齐保证（Rust 专家）**
`MaybeUninit<[u8; SIZE]>` 对齐到 1 字节。RADIO DMA 要求 PACKETPTR 字对齐（[PS §6.17.6](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html)）。也缺少内部可变性。
**修复**：使用 `UnsafeCell<[u8; SIZE]>` 加 `#[repr(C, align(4))]`。

**R4. Feature gate 允许损坏的编译状态（Rust 专家）**
`default = ["timer1"]` 意味着 `default-features = false` = 无帮助的编译错误。多个 timer feature = 冲突的 impl。
**修复**：移除 default，添加 `compile_error!` 守卫用于互斥和至少选一个。

**R5. `CRCCNF.SKIPADDR` 未显式配置（协议专家）**
esb-ng 依赖复位默认值 `1`（CRC 跳过地址）。如果此值改变，ESB CRC 静默失败。与 [errata [143]](https://docs.nordicsemi.com/bundle/errata_nrf52840_EngA/page/ERR/nRF52840/EngineeringA/latest/anomaly_840_143.html) 相关。
**修复**：在 radio init 中显式设置 `CRCCNF.SKIPADDR = 1`。

**R6. 重传尝试计数差一错误（协议专家）**
esb-ng 使用 `attempts > max`（严格大于），给出 4 次尝试而非配置的 3 次。可能是 bug。
**修复**：新实现中使用 `>=`。对照 Nordic C ESB 参考（`nrf_esb.c`）验证。

**R7. Errata [122] — DMA 缓冲必须在 RAM block 0（协议专家）**
EasyDMA 不能读取 RAM block 1（nRF52840 上 `0x2003_0000-0x2003_FFFF`）。所有数据包缓冲必须在 RAM block 0。
**修复**：确保 `PacketPool` 放在 `.bss`/`.data`（RAM block 0）。需要时使用 `#[link_section = ".data"]`。

### 重要问题（应该修复，可能导致微妙 bug）

**R8. Timer CC[0] 使用绝对值，CC[1] 使用 capture+add（协议专家）**
esb-ng 的 `set_interrupt_retransmit()`：CC[0] = 绝对值 + `tasks_clear` + `tasks_start`（从 0 开始）。`set_interrupt_ack()`：CC[1] = `tasks_capture` + 加相对偏移。计划必须文档化此不对称性。
**来源**：`esb-ng/src/peripherals.rs` 第 608-616 行。

**R9. 单一 ISR 上下文架构未文档化（协议专家）**
TIMER ISR 触发 → 设置标志 → `NVIC::pend(RADIO)` → RADIO ISR 处理 timer 和 radio 事件。所有状态机逻辑在 RADIO ISR 上下文中运行。不能拆分到两个 ISR。
**来源**：`esb-ng/src/irq.rs` 第 45-50 行。

**R10. NoAck RADIO shortcut 未文档化（协议专家）**
NoAck 包不得添加 `disabled_rxen` shortcut。Radio 在 TX END 后变为 DISABLED。立即释放缓冲，进入下一个包。
**来源**：`esb-ng/src/irq.rs` 第 215-219 行。

**R11. 最小 ACK 缓冲为 2 字节 `[0,0]`，不是 0（协议专家）**
DMA 指针必须始终指向 ≥2 有效字节：`[length(1), pid_no_ack(1)]`。零长度数组 = 越界 DMA 读取。
**来源**：`esb-ng/src/peripherals.rs` 第 342 行。

**R12. Builder 所有权模型没有静态锚点（Rust 专家）**
`isr_handle` 需要 `&'static mut` 引用到 Radio/Timer，但没有指定 `static` 容器。
**修复**：采用 esb-ng 的 `EsbBuffer` + `try_split(&'static self)` 模式，或使用 `static_cell::ConstStaticCell`。

**R13. `embassy-sync::Channel` ISR 安全性（Rust 专家）**
必须显式使用 `CriticalSectionRawMutex`。不要直接依赖 `critical-section` — 让用户提供后端（与 RMK 使用 `nrf-mpsl` 相同）。

**R14. ISR 到 async 桥接机制未指定（Rust 专家）**
计划没有解释替换 bbq2 的 maitake waker 后 ISR 如何唤醒 async 任务。`Channel::try_send()` 从 ISR 通过 `CriticalSectionRawMutex` 触发 waker。

**R15. 缺少 API 方法（RMK 专家）**
`send_no_ack()`、`stop()`/`disable()`、`is_tx_idle()`、`max_attempts_reached()` 回调 — RMK 集成全部需要。添加到 M8 API。

**R16. `PCNF0.S1INCL` 未显式文档化（协议专家）**
复位默认值 = Automatic（S1LEN>0 时 S1 在 RAM 中）。必须文档化依赖关系；改变 S1INCL 会改变 DMA 缓冲布局。

## 补充评审（独立 Rust/ESB 专家 — Round 2）

> 针对 Embassy 社区贡献和 RMK 长期集成目标的独立评审。

### 架构问题

**A1. Timer feature gate 与 Embassy 惯例不符**
Embassy 生态中外设选择通过泛型参数实现，而非 feature gate。当前 `timer0`/`timer1`/... 设计导致：一个二进制只能用一种 timer、`compile_error!` 互斥组合爆炸、与 Embassy 风格不一致。
**修复**：采用 Embassy 标准泛型模式 `pub struct Esb<'d, T: TimerInstance>`。用户传入具体 timer 外设，编译器自动约束。移除 timer feature gate。

**A2. `EsbBuffer::new()` const 约束**
`static ESB_BUF: EsbBuffer<4, 252> = EsbBuffer::new()` 要求 `EsbBuffer::new()` 是 `const fn`，但 `embassy-sync::Channel::new()` 在某些版本中不是 const。
**修复**：调研 embassy-sync 0.8 的 const 支持。如果不行，使用 `static_cell::make_static!` 宏（Embassy 示例标准模式）。

**A3. 状态机过度解耦的时序风险**
纯函数 `ptx_step()` + ISR 分派层增加几百 ns 开销。ESB ACK 窗口 ~130us，延迟需最小化。
**修复**：让状态机直接持有 radio/timer 引用并操作寄存器（与 embassy-nrf 驱动风格一致）。测试用 `#[cfg(test)]` mock peripheral。不追求过度解耦。

### Embassy 社区贡献视角

**B1. Crate 命名不一致**
仓库名 `embassy-nrf-esb`，crate 名 `nrf-esb`。Embassy 惯例：`embassy-nrf-xxx`。
**修复**：统一命名。建议 crate 也叫 `embassy-nrf-esb`，或在 README 声明命名策略和上游路径。

**B2. 缺少通用 trait 抽象**
开源项目应提供框架无关的 trait。RMK 的 `SplitReader`/`SplitWriter` 是 RMK 特定的。
**修复**：M8 后定义 `trait EsbTransport`（send/receive），RMK 在外部 impl。其他框架也能适配。

**B3. 缺少多芯片支持说明**
Plan 只提 nRF52840。社区会问 nRF52832/52833 支持。
**修复**：M0 Cargo.toml 预留 `nrf52832`/`nrf52833` feature（暂不实现），README 说明支持路径。nRF52832 无 errata [122] 问题。

**B4. 缺少可直接编译的 example 模板**
Embassy 社区看 crate 先看 examples 能否 `cargo build`。
**修复**：M0 包含 `.cargo/config.toml` + `memory.x` + `build.rs` 模板。

### ESB 协议补充

**C1. 地址配置 API 未设计**
ESB 地址 = 4 字节 BASE + 1 字节 PREFIX。Pipe 0-1 独立 BASE，Pipe 2-7 共享 Pipe 1 的 BASE。需在 M1 设计 `EsbAddresses` 类型。
**修复**：M1 添加 `EsbAddresses` builder，验证地址约束。

**C2. 数据速率配置缺失**
ESB 支持 1Mbps/2Mbps，影响 ramp-up time 和 timing 计算。Plan 提到 `fast-ru` 但无数据速率选择。
**修复**：Config 添加 `data_rate: DataRate`（`OneMbps`/`TwoMbps`），影响 RADIO MODE 寄存器和 timing 常量。

**C3. `suspend()` 调用时机约束不足**
`suspend()` 是 async API，用户从任务上下文调用，但此时状态机可能处于事务中途。
**修复**：`suspend()` 实现需要：(1) 禁用 RADIO IRQ 阻止新事务；(2) 等待/超时当前事务完成；(3) 保存状态。MPSL 时隙结束回调中可能没时间等待 — 提供 `try_suspend()` 返回 `Err(Busy)`。

**C4. PRX 时隙可靠性是架构问题**
如果 dongle（PRX 端）也需要 BLE，则 PRX 在 timeslot 中必须整个时隙监听。需明确 RMK 架构：dongle 是 USB 专用（独占 PRX）还是也需 BLE。
**修复**：M10 中明确 RMK 的 PRX 部署策略。建议 dongle 跑独占模式 PRX（USB 专用），键盘端 PTX + BLE 共存。

### Rust 安全性补充

**D1. AtomicU8 状态转换需要明确 Ordering**
ISR 和任务上下文同时访问 slot 状态，ordering 关键：
- ISR: `in_dma → rx_queued` 用 `Release`（DMA 数据可见）
- 任务: `rx_queued → free` 用 `Acquire`（读取 DMA 数据）
- 任务: `free → tx_queued` 用 `Release`（payload 写入可见）
- ISR: `tx_queued → in_dma` 用 `Acquire`（读取 payload）
**修复**：M4 中明确每个转换的 ordering。不要全用 `SeqCst`（浪费）或 `Relaxed`（不安全）。

**D2. `PacketPool` 的 `Sync` 实现需要安全论证**
`UnsafeCell` 不是 `Sync`，但 static 要求 `Sync`。需 `unsafe impl Sync`。
**修复**：M4 中明确 safety 论证 — AtomicU8 保证同一时刻只有一方访问同一 slot。代码中写 `// SAFETY:` 注释。

**D3. Cargo.lock 应该提交**
Embassy 生态中有 examples 的 crate 都提交 Cargo.lock（CI 可复现）。
**修复**：从 `.gitignore` 移除 `Cargo.lock`。

### 时间线修正

| 里程碑 | 原估 | 修正估 | 原因 |
|--------|------|--------|------|
| M2 | 2 天 | 3-4 天 | nrf-pac 0.3 迁移坑比预期多 |
| M4 | 1-2 天 | 2-3 天 | Atomic ordering + UnsafeCell 安全论证 |
| M9 | 3-5 天 | 5-10 天 | 硬件调试永远比想象的久 |
| M10 | 5-7 天 | 7-14 天 | MPSL 集成无 Rust 参考，全新领域 |
| **总计** | **~35 天** | **~45-55 天** | 更现实的估计 |

### 缺失项

| 项目 | 建议归属里程碑 | 说明 |
|------|---------------|------|
| 电源管理策略 | M8 | RADIO 空闲时 disable，键盘省电 |
| 错误恢复机制 | M7 | RADIO 卡死（EVENTS_DISABLED 不触发）的 watchdog/timeout |
| defmt 日志策略 | M7 | ISR 中不能 log，但状态转换需 trace 级记录 |
| CI 配置详情 | M0 | `cargo check` + `cargo test` + `cargo clippy` 最低配置 |
| 版本策略 | M0 | embassy-nrf 可能需要 git dep（crates.io 不允许），影响发布策略 |

---

## 设计优先级

1. **自己用（RMK）** — 实用优先，API 必须服务于 RMK 的分体键盘架构
2. **开源友好** — 清洁的 API 边界、Embassy 约定、公共 API 不暴露 PAC 类型。方便后续重构为 `embassy-nrf-esb` 或合并为 embassy-nrf 模块
3. **MPSL 硬需求** — BLE+ESB 并发是必须功能，不是可选。Suspend/resume 是核心 API，不是 feature-gated 后加模块

## 战略决策

| 决策 | 选择 | 理由 |
|------|------|------|
| Fork 还是新建 | **新建仓库**，esb-ng 作参考 | 每个模块都需要根本性重构；干净的 API 比保留 git 历史更重要 |
| Rust 版本 | **2021** | 最大兼容性；2024 对 no_std HAL crate 无额外收益 |
| 缓冲方案 | **embassy-sync**（不用 bbq2） | RMK 已依赖它；bbq2 的 maitake-sync 是并行异步运行时 |
| 定时器默认值 | **TIMER1**（泛型 `<T: EsbTimer>`） | TIMER0 被 MPSL 独占；TIMER2 被 Gazell 使用；参数化提供灵活性 |
| MPSL 集成 | **核心 suspend/resume API + 时隙适配器** | Suspend/resume 是核心设计的一部分（MPSL 时隙和 BLE/ESB 热切换都需要）。时隙适配器是独立模块但不是后加想法 |
| MPSL 时隙方法 | **Rust 自定义处理程序**（不用 Zephyr ESB 库） | 纯 Rust ESB；不能使用 Zephyr 的 C 库。移植 too1/inductivekickback 模式 |
| ESB 挂起/恢复 | **每次时隙完全重新初始化**（非轻量级挂起） | 两个参考实现都使用完全重新初始化；验证稳定；避免微妙的状态 bug |
| PID 持久化 | **挂起前保存，恢复后还原** | ESB 协议使用 2 位 PID 进行重复检测；丢失 PID 导致接收端拒绝 |
| PAC 依赖 | **通过 `embassy-nrf::pac` 重导出** | 不直接依赖 `nrf-pac`；使用 embassy-nrf 的重导出以版本对齐 |
| ISR 绑定 | **用户选择**：`bind_interrupts!` 或手动 `#[interrupt]` | 提供 `on_radio_interrupt()` 方法；用户可用 Embassy 宏或 RMK 的 AtomicU8 dispatch |
| 公共 API | **不暴露 PAC 类型** | 用 newtype 包装所有 PAC 类型；版本升级不破坏用户 |

## Crate 结构

```
embassy-nrf-esb/                  （本仓库，crate 名称：nrf-esb）
├── Cargo.toml
├── src/
│   ├── lib.rs                    — 重新导出、Error、Config、常量
│   ├── radio.rs                  — RADIO 寄存器访问（参考: esb-ng peripherals.rs）
│   ├── timer.rs                  — EsbTimer trait + TIMER1/2/3/4 实现
│   ├── state_machine.rs          — PTX/PRX 状态机（参考: esb-ng irq.rs）
│   ├── buffer.rs                 — 静态数据包池 + embassy-sync 通道
│   ├── payload.rs                — EsbHeader、PayloadR、PayloadW 类型
│   ├── suspend.rs                — ESB 状态保存/恢复（核心 API，非 feature-gated）
│   ├── async_driver.rs           — EsbPtx、EsbPrx、EsbBuilder（Embassy async API）
│   ├── isr.rs                    — RADIO + TIMER ISR 胶水层
│   └── mpsl_timeslot.rs          — MPSL 时隙适配器（依赖 suspend.rs）
├── examples/
│   ├── ptx_blinky.rs             — PTX 发送计数器，ACK 时 LED 闪烁
│   ├── prx_blinky.rs             — PRX 接收，LED 闪烁，发送 ACK payload
│   └── ptx_ble_concurrent.rs     — PTX + BLE 并发（通过 MPSL 时隙）
├── .github/workflows/ci.yml
└── README.md
```

**模块依赖图**：
```
payload ──────────────────────────────────┐
radio ──── state_machine ──── isr ──── async_driver
timer ──── ┘               └── suspend ──┤
buffer ───────────────────────────────────┘
                                         └── mpsl_timeslot
```

`suspend.rs` 是**核心模块**（不在 feature gate 后面）。它定义：
- `EsbSavedState` — PID、CRC、重传计数、活跃管道、待发送 TX
- `suspend()` — 保存状态、禁用 RADIO、停止定时器
- `restore()` — 完整 ESB 重新初始化、恢复 PID、从保存状态恢复

这服务两个消费者：
1. **`mpsl_timeslot.rs`** — 跨 MPSL 时隙的保存/恢复（Phase 7）
2. **RMK BLE/ESB 热切换** — 切换 radio 模式时的保存/恢复（Phase 4 AtomicU8 dispatch）

## 里程碑

### M0: 仓库骨架（0.5 天）

**交付物**：可编译 `thumbv7em-none-eabihf` 的空 crate

**Cargo.toml**：
```toml
[package]
name = "embassy-nrf-esb"
version = "0.1.0"
edition = "2021"
rust-version = "1.75"

[dependencies]
embassy-nrf = { version = "0.10", features = ["nrf52840"] }
embassy-sync = "0.8"
cortex-m = { version = "0.7", default-features = false }
static_cell = "2"
defmt = { version = "0.3", optional = true }
nrf-mpsl = { version = "0.1", optional = true }

[features]
# 芯片选择（暂只实现 nrf52840，预留其他）
nrf52840 = ["embassy-nrf/nrf52840"]
nrf52833 = ["embassy-nrf/nrf52833"]
nrf52832 = ["embassy-nrf/nrf52832"]
# 功能选项
fast-ru = []
mpsl = ["dep:nrf-mpsl"]
defmt = ["dep:defmt", "embassy-nrf/defmt"]
# 注意：Timer 通过泛型 <T: TimerInstance> 选择，无需 feature gate (A1)
# PAC 通过 embassy-nrf::pac 访问（不直接依赖 nrf-pac）
```

**Timer 选择方式**（修复 R4 + A1）：不再使用 feature gate 互斥，改为 Embassy 标准泛型模式：
```rust
// 用户代码中：
let (ptx, isr) = esb_init::<peripherals::TIMER1>(timer1, radio, config);
// 编译器自动约束，无 feature 冲突
```

**芯片 feature 守卫**（在 `lib.rs` 中）：
```rust
#[cfg(not(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832")))]
compile_error!("One chip feature must be enabled (nrf52840, nrf52833, or nrf52832)");
```

**Example 模板**（修复 B4）：M0 交付物包含 `.cargo/config.toml` + `memory.x` + 最小 example 骨架，确保 `cargo build --example ptx_blinky --features nrf52840` 直接通过。

**版本策略说明**：`embassy-nrf 0.10` 若未发布，暂用 git 依赖。发布到 crates.io 前需切换为版本依赖。README 中说明此限制。

**验证**：`cargo check --target thumbv7em-none-eabihf`

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| nrf-pac 0.3 feature flag 名称错误 | 低 | 低 | 写 Cargo.toml 前查阅 nrf-pac 文档确认确切的 feature 名称 |
| 依赖版本与 RMK 冲突 | 中 | 中 | 锁定到 RMK 使用的相同版本；`cargo tree` 验证无重复 |

**回退方案**：删除仓库重来。没有代码损失。

---

### M1: 核心类型 — Error、Config、Payload、Header（1-2 天）

**文件**：`src/lib.rs`、`src/payload.rs`

**参考**：`esb-ng/src/lib.rs`（Error/Config）、`esb-ng/src/payload.rs`（EsbHeader）

**与 esb-ng 的关键变化**：
- 从 payload 类型中移除 bbq2 依赖
- `PayloadR` / `PayloadW` 变为 `&[u8]` / `&mut [u8]` 切片 + header 包装
- 保持 4 字节 header 布局：`[rssi, pipe, length, pid_no_ack]`（硬件依赖）

**Header DMA 布局**：
```rust
/// 不要重新排序这些字段。DMA payload offset 跳过
/// 字节 0-1（rssi, pipe）——仅软件字段。字节 2-3
/// （length, pid_no_ack）是实际的 RADIO DMA header。
#[repr(C)]
struct EsbHeader {
    rssi: u8,       // [SW] 字节 0 — 不传输
    pipe: u8,       // [SW] 字节 1 — 不传输
    length: u8,     // [HW] 字节 2 — RADIO PCNF0.LFLEN
    pid_no_ack: u8, // [HW] 字节 3 — RADIO PCNF0.S1LEN
}
const _: () = assert!(EsbHeader::dma_payload_offset() == 2);
```

**EsbAddresses 类型**（修复 C1）：
```rust
/// ESB 地址配置。Pipe 0-1 有独立 BASE，Pipe 2-7 共享 Pipe 1 的 BASE。
pub struct EsbAddresses {
    pub base0: [u8; 4],       // Pipe 0 的 4 字节 BASE 地址
    pub base1: [u8; 4],       // Pipe 1-7 的 4 字节 BASE 地址
    pub prefix: [u8; 8],      // 每个管道的 1 字节 PREFIX
    pub pipe_count: u8,       // 启用的管道数 (1-8)
}
```
Builder 验证：`pipe_count` 1-8、prefix 唯一性检查。

**DataRate 配置**（修复 C2）：
```rust
pub enum DataRate {
    OneMbps,   // RADIO MODE = Nrf_1Mbit
    TwoMbps,   // RADIO MODE = Nrf_2Mbit
}
```
影响：RADIO MODE 寄存器、ramp-up time（1M: 130us/40us-fast, 2M: 130us/40us-fast）、PCNF0.LFLEN 配置。

**Config 验证**（对照 [Nordic ESB User Guide](https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html)）：
- `ack_timeout >= 44` us
- `retransmit_delay > ack_timeout + 62` us（注意：用 `>`，不是 `>=`）
- `retransmit_delay > RAMP_UP_TIME`（140 us 正常，40 us fast-ru）
- `max_payload <= 252` 字节
- `data_rate` 与 `fast-ru` feature 的组合影响 timing 常量

**主机端测试**：
- Header builder 往返测试、验证（pipe 0-7, pid 0-3, length 0-252）
- Config 验证（边界值测试）
- Config builder 链式调用

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| Header 布局与硬件不匹配 | 低 | 严重 | 使用 esb-ng 的确切布局；添加编译时断言验证 dma_payload_offset=2 |
| Config 边界过于限制 | 低 | 低 | 遵循 Nordic ESB 默认值；HW 测试后按需放宽 |
| `PayloadR`/`PayloadW` 生命周期复杂度 | 中 | 中 | 先用拥有的 `[u8; 252]` 数组；后续按需添加切片包装 |

**回退方案**：回退到 esb-ng 的 payload 类型并包装它们。

---

### M2: Radio 寄存器访问（2 天）

**文件**：`src/radio.rs`

**参考**：`esb-ng/src/peripherals.rs` 第 60-498 行

**官方文档**：[RADIO 外设 PS §6.17](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html)、[EasyDMA PS §6.17.6](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html)

**nrf-pac 0.1 → 0.3 迁移**：
- API 几乎一致（`write(|w| ...)`、`read()`、`modify()`）
- `Radio` 在 0.3 中是 `Copy`（指针包装器）— 所有权模型的根本变化
- 与 buffer 类型解耦：radio 层只跟踪 DMA 指针

**结构体**：
```rust
pub struct EsbRadio {
    radio: pac::RADIO,
    last_crc: [u16; 8],
    last_pid: [u8; 8],
}
```

关键方法：`init()`、`transmit()`、`prepare_for_ack()`、`check_ack()`、`start_receiving()`、`check_packet()`、`complete_rx_ack()`

**关键寄存器写入**（必须显式设置，不依赖复位默认值）：
```rust
// CRCCNF: 必须显式设置 SKIPADDR=1 (R5)
radio.crccnf().write(|w| {
    w.set_len(Len::TWO);
    w.set_skipaddr(true);  // CRC 不包含地址字段（ESB 标准）
});

// PCNF0: S1INCL=Automatic（复位默认），S1LEN=3
radio.pcnf0().write(|w| {
    w.set_lflen(len_bits);
    w.set_s1len(3);
    // S1INCL 未设置：依赖复位默认值（Automatic）
    // 不要改变 S1INCL，除非同时更新 DMA 缓冲布局 (R16)
});
```

**compiler_fence 放置**（协议评审 R13）：
1. `tasks_txen()` / `tasks_rxen()` 之前 — `Release`（确保 DMA 缓冲写入可见）
2. `crcstatus().read()` 之后 — `Acquire`（确保 DMA 写入对 CPU 可见）
3. PACKETPTR 写入周围 — 之前 `Release`，之后 `Acquire`

**事件清除顺序**：在启用 shortcuts/触发 tasks 之前清除事件。如果 EVENTS_DISABLED 已设置，shortcut 会立即触发。

**验证**：`cargo check --target thumbv7em-none-eabihf`

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| nrf-pac 0.3 特定寄存器 API 变化 | 中 | 高 | 并排对比 esb-ng 的 0.1 调用与 0.3 API；逐个寄存器写入测试 |
| DMA 周围缺少内存屏障 | 中 | 严重 | 遵循上述 fence 放置检查清单 |
| RADIO shorts 配置错误 | 低 | 高 | 从 esb-ng 复制确切的 shorts；NoAck 模式不添加 `disabled_rxen`（R10） |
| CRC 配置不匹配 | 低 | 高 | 使用 ESB 标准：CRC=2 字节，多项式=0x11021，SKIPADDR=1（R5） |
| 隐式寄存器默认值变化 | 低 | 严重 | 显式设置 CRCCNF.SKIPADDR=1，文档化 PCNF0.S1INCL 依赖（R5, R16） |
| DMA 缓冲在 RAM block 1 | 低 | 严重 | Errata [122]：所有 PACKETPTR 目标必须在 RAM block 0（R7） |

**回退方案**：如果 0.3 迁移受阻，用兼容层包装 esb-ng 的 peripherals.rs。

---

### M3: 定时器抽象（1 天）

**文件**：`src/timer.rs`

**参考**：`esb-ng/src/peripherals.rs` 第 500-684 行

**官方文档**：[TIMER 外设 PS §6.24](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/timer.html)

**关键变化**：移除 `PtrTimer::take()` unsafe 单例。采用 Embassy 标准泛型模式（R1 + A1 修复）。

```rust
/// Timer 外设 trait — Embassy 泛型模式
pub trait TimerInstance: sealed::Sealed + 'static {
    fn regs() -> pac::timer::Timer;
}

// 为每个支持的 timer 实现（无 feature gate，靠泛型约束）
impl TimerInstance for peripherals::TIMER1 { ... }
impl TimerInstance for peripherals::TIMER2 { ... }
impl TimerInstance for peripherals::TIMER3 { ... }
impl TimerInstance for peripherals::TIMER4 { ... }
// TIMER0 不实现 — 被 MPSL 独占

/// 初始化时接受拥有的外设，保证单例语义。
/// ISR 时通过 `T::regs()` 访问（T 是 Copy 的 PAC 类型指针）。
pub struct EsbTimer<T: TimerInstance> {
    _phantom: PhantomData<T>,
}

impl<T: TimerInstance> EsbTimer<T> {
    pub fn new(_timer: T) -> Self {
        // 消耗外设所有权，保证不会被其他代码使用
        Self { _phantom: PhantomData }
    }

    /// ISR 安全：T::regs() 返回 PAC 指针（Copy），无需 &'static mut
    pub(crate) fn regs(&self) -> pac::timer::Timer {
        T::regs()
    }
}
```

**优势**：无 feature gate 互斥、编译时 timer 选择、与 Embassy 驱动风格一致。

**定时器模式**：32 位，预分频=4（1MHz）。不是 16 位。([PS §6.24](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/timer.html))

**CC 通道语义**（R8 — 必须文档化）：
- **CC[0]**（重传）：绝对值。`tasks_clear()` + `tasks_start()` — 定时器从 0 开始计数。值 = `retransmit_delay - RAMP_UP_TIME`。
- **CC[1]**（ACK 超时）：相对于当前计数器。`tasks_capture(1)` 读当前计数，然后 CC[1] += `ack_timeout + RAMP_UP_TIME`。

**验证**：`cargo check --target thumbv7em-none-eabihf`

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| Timer CC 通道语义与 esb-ng 不同 | 低 | 高 | CC[0] = 绝对值（clear+start），CC[1] = 相对值（capture+add）。精确文档化（R8） |
| 没有 singleton guard 的静态定时器访问 | 中 | 严重 | 使用 `EsbTimerHandle` 从 embassy 外设转换；`regs()` 仅为 `pub(crate)`（R1） |
| 1MHz 预分频计算错误 | 低 | 中 | prescaler=4（16MHz/16=1MHz），32 位模式。直接复制 |

**回退方案**：如果 trait 设计有问题，改用宏驱动的定时器选择。

---

### M4: 数据包缓冲（1-2 天）

**文件**：`src/buffer.rs`

**用 embassy-sync 替代 bbq2**：静态数据包池 + 显式 DMA 安全 + `embassy_sync::Channel<CriticalSectionRawMutex, ...>`

```rust
#[repr(C, align(4))]  // 字对齐用于 RADIO DMA (R3, R7)
pub struct Packet<const SIZE: usize> {
    data: UnsafeCell<[u8; SIZE]>,
}

pub struct PacketPool<const N: usize, const SIZE: usize> {
    storage: [Packet<SIZE>; N],
    state: [AtomicU8; N],  // 0=free, 1=tx_queued, 2=in_dma, 3=rx_queued (R2)
    tx_queue: Channel<CriticalSectionRawMutex, usize, N>,
    rx_queue: Channel<CriticalSectionRawMutex, usize, N>,
}
```

**关键设计决策**：
- `UnsafeCell` + `align(4)` 替代 `MaybeUninit` — 保证字对齐和内部可变性（R3）
- `AtomicU8` 每个槽位状态 — 跟踪 `in_dma` 防止 use-after-free（R2）
- 显式使用 `CriticalSectionRawMutex`（不是 `RawMutex` 别名）— ISR 安全，无歧义（R13）
- 所有缓冲在 RAM block 0（errata [122]）— 需要时用 `#[link_section]` 验证（R7）
- 最小 ACK 缓冲 = 2 字节 `[0,0]` — DMA 指针始终指向 ≥2 有效字节（R11）

**Atomic Ordering 规范**（修复 D1）：
```rust
// 状态转换及对应 ordering：
// 任务 → ISR 方向（任务写数据，ISR 读数据）：
//   free → tx_queued:  store(Release)  — payload 写入对 ISR 可见
// ISR → 任务方向（ISR 写 DMA 数据，任务读数据）：
//   in_dma → rx_queued: store(Release) — DMA 接收数据对任务可见
// ISR 内部：
//   tx_queued → in_dma: load(Acquire) + store(Relaxed) — 读取 payload
// 任务内部：
//   rx_queued → free:   load(Acquire) + store(Relaxed) — 读取 RX 数据后释放
```

**`unsafe impl Sync` 安全论证**（修复 D2）：
```rust
// SAFETY: PacketPool 通过 AtomicU8 状态保证同一时刻只有一方（ISR 或任务）
// 访问同一个 slot 的 UnsafeCell。状态转换使用正确的 memory ordering
// 确保跨上下文的数据可见性。
unsafe impl<const N: usize, const SIZE: usize> Sync for PacketPool<N, SIZE> {}
```

**主机端测试**：池分配/释放、队列 FIFO 顺序、状态转换、对齐断言

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| 池释放时 DMA 指针失效 | 高 | 严重 | `AtomicU8` 状态：PACKETPTR 写入前转为 `in_dma`，ISR 确认 RADIO 空闲后才回 `free`（R2） |
| embassy-sync Channel ISR 安全性 | 低 | 高 | `CriticalSectionRawMutex` 是 ISR 安全的（嵌套 `interrupt::free`）。不要用 `ThreadModeRawMutex`（R13） |
| 负载下池耗尽 | 中 | 中 | 可配置池大小；满时 `try_send` 返回错误；向 async API 反压 |
| DMA 缓冲在 RAM block 1 | 低 | 严重 | Errata [122]：验证 `PacketPool` 地址 < 0x2003_0000。需要时用 `#[link_section]`（R7） |
| 对齐 < 4 字节 | 低 | 严重 | `#[repr(C, align(4))]` + 编译时 `assert!(align_of >= 4)`（R3） |

**回退方案**：如果 `embassy-sync::Channel` 在 ISR 上下文中有问题，用 `heapless::spsc::Queue` 作为后备。

---

### M5: PTX 状态机（2-3 天）

**文件**：`src/state_machine.rs`

**参考**：`esb-ng/src/irq.rs` 第 155-310 行

**关键架构变化**（修正 A3 — 不过度解耦）：
状态机直接持有 radio/timer 引用并操作寄存器，而非返回"动作"让 ISR 分派。这减少 ISR 延迟，与 embassy-nrf 驱动风格一致。测试用 `#[cfg(test)]` mock peripheral。
```rust
/// 状态机直接操作硬件（非纯函数）— 最小化 ISR 路径延迟
pub struct PtxStateMachine<T: TimerInstance> {
    radio: EsbRadio,
    timer: EsbTimer<T>,
    state: StatePTX,
    // ...
}

impl<T: TimerInstance> PtxStateMachine<T> {
    /// 从 RADIO ISR 调用 — 读事件、转换状态、写寄存器，全在内部完成
    pub(crate) fn handle_radio_event(&mut self, pool: &PacketPool) { ... }
}
```

主机端测试策略：提取纯逻辑部分为可测函数，但 ISR 入口直接操作硬件。

**状态**：esb-ng 有 5 个 PTX 状态：`IdleTx`、`TransmitterTx`、`TransmitterTxNoAck`、`TransmitterWaitAck`、`TransmitterWaitRetransmit`。

**定时器计算不对称性**（R8）：
```
重传延迟: config.retransmit_delay - RAMP_UP_TIME  （减去：radio 重新启用）
ACK 超时:  config.ack_timeout + RAMP_UP_TIME       （加上：radio ramp 到 RX）
```

**重传尝试检查**（R6）：使用 `>=` 不是 `>`：
```rust
if self.attempts >= self.config.maximum_transmit_attempts {
    // 丢弃包，报告 MaximumAttempts
}
```

**NoAck 路径**（R10）：`no_ack=true` 时，不添加 `disabled_rxen` shortcut。Radio 在 TX END 后变为 DISABLED。立即释放 TX 缓冲，进入下一个包。

**单一 ISR 上下文**（R9）：TIMER ISR 触发 → 设置 timer_flag → `NVIC::pend(RADIO)` → RADIO ISR 处理 timer 和 radio 事件。所有状态机逻辑只在 RADIO ISR 上下文中运行。

**主机端测试**：
- 使用 mock radio/timer 事件的状态转换
- 重传计数器、超过最大重试次数（验证用 `>=` 不是 `>`）
- 收到 ACK → 状态重置
- NoAck 路径（不请求 ACK，验证 `disabled_rxen` 未设置）

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| esb-ng → nrf-esb 映射中遗漏状态转换 | 中 | 高 | 列举 esb-ng 所有 PTX 转换（共 9 个），验证每个都有对应的 match 分支 |
| Ramp-up 时间补偿错误 | 中 | 高 | 重传：`-RAMP_UP`，ACK：`+RAMP_UP`。复制 esb-ng 的精确计算（R8） |
| ACK 超时窗口太窄 | 中 | 高 | 默认：wait_for_ack_timeout=120us + RAMP_UP_TIME=140us = 260us。HW 验证 |
| 重传差一错误 | 低 | 中 | 使用 `>=` 做尝试检查，不是 `>`（R6） |

**回退方案**：直接复制 esb-ng 的 `irq.rs` PTX handler 并包装。

---

### M6: PRX 状态机（2-3 天）

**文件**：`src/state_machine.rs`（同一文件，`prx_step` 函数）

**参考**：`esb-ng/src/irq.rs` 第 312-425 行

esb-ng 有 4 个 PRX 状态：`IdleRx`、`Receiver`、`TransmittingAck`、`TransmittingRepeatedAck`。

**重复检测**：使用 esb-ng 的精确 CRC+PID 检查：`(last_crc[pipe] == crc) && (last_pid[pipe] == pid)`。8 个管道条目。

**最小 ACK 缓冲**（R11）：回退 ACK = `[0u8; 2]`（2 字节：length + pid_no_ack）。

**主机端测试**：
- CRC 失败 → 自动重启
- 重复包检测（相同 CRC + PID）
- 空 ACK 回退（无 TX 排队时）
- NoAck 包处理

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| 重复检测假阴性 | 中 | 高 | 使用精确 CRC+PID 检查。每个有效 RX 更新 |
| ACK TX 时序违规 | 中 | 高 | RX 期间 `disabled_txen=false`。TX 必须在 RX 完成后约 130us 内启动 |
| PRX 在 ACK TX 后未返回 RX | 中 | 高 | 状态机必须始终转换回 `Receiver`；mock 测试验证 |
| NoAck 包未传递给应用 | 低 | 中 | NoAck 包投递到 rx_queue 但不触发 ACK TX |

**回退方案**：同 M5 — 如果提取失败，复制 esb-ng 的 PRX handler。

---

### M7: ISR 胶水层（1 天）

**文件**：`src/isr.rs`

用户在其应用中提供 ISR 包装器：
```rust
#[embassy_nrf::pac::interrupt]
fn RADIO() {
    ESB_ISR.on_radio_interrupt();
}

#[embassy_nrf::pac::interrupt]
fn TIMER1() {
    // 最小 TIMER ISR：设置标志，pend RADIO ISR (R9)
    ESB_ISR.on_timer_interrupt();
    // 内部：cortex_m::peripheral::NVIC::pend(Interrupt::RADIO)
}
```

**单一 ISR 上下文架构**（R9）：所有 ESB 状态机逻辑在 RADIO ISR 中运行。TIMER ISR 最小化（清除事件、设置标志、pend RADIO ISR）。避免 ISR 间同步问题。

ISR 优先级：RADIO 和 TIMER 必须为 P0（最高）以保证正确的 ESB 时序。

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| ISR 优先级与 MPSL 冲突 | 中 | 高 | 独占模式下 RADIO/TIMER P0 没问题。MPSL 模式下，MPSL 以 P0 拥有 RADIO，通过 signal callback 分派 |
| ISR 处理程序太慢（栈溢出） | 低 | 严重 | 保持 ISR 最小：读事件、调状态机、写寄存器、返回。无分配、无日志 |
| 编译器内联破坏 ISR 时序 | 低 | 高 | 标记关键函数 `#[inline(always)]`；需要时用 `cargo-asm` 验证 |

**回退方案**：如果纯函数方法导致时序问题，使用 esb-ng 的方法（全部在 ISR 内完成）。

---

### M8: Embassy Async API（2-3 天）

**文件**：`src/async_driver.rs`

**公共 API**：
```rust
// 静态缓冲锚定 ISR 状态 (R12 修复)
static ESB_BUF: EsbBuffer<4, 252> = EsbBuffer::new();

// 初始化：try_split 接受 &'static self，返回 'static handles
let (ptx, isr_handle) = ESB_BUF.try_split(timer, radio, &addresses, Config::default())?;

// PTX 使用
ptx.send(pipe, &payload).await?;           // 带 ACK 发送（默认）
ptx.send_no_ack(pipe, &payload).await?;    // 不带 ACK 发送 (R15)
let ack = ptx.try_receive();                // 检查 ACK payload
let dropped = ptx.max_attempts_reached();   // 检查是否有包被丢弃 (R15)

// PRX 使用
let (prx, isr_handle) = ESB_BUF.try_split(timer, radio, &addresses, Config::default())?;
prx.start_listening();
let packet = prx.receive().await;           // 等待数据包
prx.send_ack_payload(pipe, &response).await?;  // 排队 ACK payload

// 清理（Phase 7 BLE/ESB 切换需要）
ptx.stop();  // 禁用 RADIO，释放资源 (R15)

// Suspend/restore（核心 API，用于 MPSL 时隙和 BLE/ESB 热切换）
let state = ptx.suspend()?;          // 保存 PID，禁用 RADIO，停止定时器
// ... MPSL 运行 BLE，或用户切换模式 ...
ptx.restore(&state, &addresses)?;    // 完整重新初始化，恢复 PID，继续
```

**ISR 到 async 桥接**（R14）：ISR 调用 `Channel::try_send()` 到 `rx_queue`，通过 `CriticalSectionRawMutex` 触发 Embassy waker。`receive().await` 自动唤醒。不需要单独的 `Signal`。

**验证**：使用 Embassy executor 编译通过，API 可用性（ping-pong 示例 <50 行）

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| `send().await` 阻止 ISR 发送 | 中 | 高 | `send()` 入队到 `tx_queue` 并通过 `NVIC::pend(RADIO)` pend ISR。ISR 从队列获取 |
| `receive().await` 永远不唤醒 | 中 | 中 | ISR `Channel::try_send()` 通过 CriticalSectionRawMutex 触发 waker（R14） |
| Builder 消费 timer+radio 但 ISR 需要静态访问 | 中 | 严重 | `EsbBuffer::try_split(&'static self)` 模式 — 缓冲是 `static`，handles 是 `'static`（R12） |
| 缺少 RMK 集成所需方法 | 低 | 中 | 添加 `send_no_ack()`、`stop()`、`max_attempts_reached()`（R15） |

**回退方案**：如果 async 集成受阻，提供阻塞 API 作为后备。

---

### M8.5: Suspend/Resume API（1-2 天）

**文件**：`src/suspend.rs`

**为什么是核心模块**：MPSL 时隙和 BLE/ESB 热切换都需要同样的能力 — 干净地保存 ESB 状态、释放 RADIO、然后恢复继续。这不是 feature-gated 后加模块；而是独占模式和 MPSL 模式用户都必需的基础 API。

**参考**：`too1/ncs-esb-ble-mpsl-demo/app_esb.c`（`app_esb_suspend`/`app_esb_resume`）、`inductivekickback/ncs_ble_esb_demo/proprietary_rf.c`（`esb_get_pid`/`esb_set_pid`）

**关键类型**：
```rust
/// 跨 suspend/resume 周期保存的状态。
/// 调用方（MPSL 时隙处理程序或 BLE/ESB 切换代码）负责存储。
#[derive(Clone, Copy)]
pub struct EsbSavedState {
    pub pid: [u8; 8],              // 每管道 PID（2 位计数器，持久化）
    pub current_pipe: u8,           // PTX 活跃管道
    pub retransmit_count: u8,       // 当前重试计数
    pub last_crc: [u16; 8],        // 每管道 CRC（用于重复检测）
    pub state: SavedProtocolState,  // Idle, MidTx, MidRx
}

pub enum SavedProtocolState {
    Idle,                           // 可安全挂起
    MidTransaction { attempt: u8 }, // 重传中；保存尝试计数
}
```

**核心 API（EsbPtx/EsbPrx 上）**（修复 C3 — 时序约束）：
```rust
impl EsbPtx {
    /// 尝试挂起 — 如果状态机正在事务中返回 Err(Busy)。
    /// 用于 MPSL 时隙结束回调（无法等待）。
    pub fn try_suspend(&self) -> Result<EsbSavedState, Error>;

    /// 异步挂起 — 等待当前事务完成（或超时）后挂起。
    /// 用于用户主动切换模式（可以等待）。
    pub async fn suspend(&self) -> Result<EsbSavedState, Error>;

    /// 完整 ESB 重新初始化 + 恢复保存的状态。
    /// RADIO 电源循环，地址重新配置，PID 恢复。
    pub fn restore(&mut self, state: &EsbSavedState, addresses: &EsbAddresses) -> Result<(), Error>;
}
```

**`suspend()` 内部时序**：
1. 禁用 RADIO IRQ（阻止新事务启动）
2. 检查状态机是否 Idle → 如果是，直接保存
3. 如果事务中途：`try_suspend` 返回 `Err(Busy)`；`suspend().await` 等待 ISR 完成当前事务（通过 Signal 通知）
4. 超时机制：如果等待 >1ms（异常），强制 disable RADIO + 丢弃当前包
```

**suspend() 序列**（匹配 too1 `app_esb_suspend`）：
1. 禁用 RADIO IRQ（`NVIC::disable_irq(RADIO)`）
2. 设置 RADIO SHORTS = 0，触发 TASKS_DISABLE，自旋等待 EVENTS_DISABLED
3. 停止 TIMER
4. 清除所有 RADIO 中断（`INTENCLR = 0xFFFFFFFF`）
5. 从 `last_pid[]` 保存 PID，从 `last_crc[]` 保存 CRC，保存尝试计数
6. 清除挂起的 RADIO IRQ

**restore() 序列**（匹配 too1 `app_esb_resume`）：
1. RADIO 电源循环：`POWER = Off; POWER = On`
2. 完整 `EsbRadio::init()`（寄存器设置，CRC 配置，shorts）
3. 配置地址
4. 恢复 PID 到 `last_pid[]`，恢复 CRC 到 `last_crc[]`
5. 如果有待发送 TX 则从队列取出
6. 重新启用 RADIO IRQ

**主机端测试**：save/restore 往返、PID 跨 100 个周期持久化

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| 事务中途挂起损坏状态 | 中 | 高 | 只在 ISR 上下文中状态机处于 Idle 时挂起。如果处于 TX 中途，先完成事务或丢弃包 |
| 挂起时 PID 丢失导致重复拒绝 | 低 | 严重 | 显式保存/恢复 `last_pid[pipe]`。用计数器测试：1000 个包跨 100 个 suspend/resume 周期 |
| RADIO 电源循环时序 | 低 | 中 | too1 示例直接 Off→On。按 PS 验证无需稳定时间 |
| Restore 重新初始化开销 | 低 | 低 | 约 20 个寄存器写入 ≈ 几微秒。对时隙转换可接受 |

---

### M9: 硬件验证 — 独占模式（3-5 天）

**硬件**：E104-BT5040U（PRX）+ nice!nano（PTX），均为 nRF52840

**逐步验证**：

| 步骤 | 测试 | 通过标准 |
|------|------|----------|
| 1 | Radio 初始化 | RTT/defmt 显示正确频率、地址、RX 模式 |
| 2 | 单个 PTX→PRX 数据包 | Payload 一致，零损坏 |
| 3 | ACK 往返 | PTX 收到 PRX 的 ACK payload |
| 4 | 1000 包连续流 | 1m 距离 0% 丢包，5m 距离 <1% |
| 5 | 重传测试 | 关闭 PRX → PTX 报告 MaximumAttempts；打开 → 恢复 |
| 6 | 多管道（2 管道） | 两个管道都正确接收 |
| 7 | 延迟测量（GPIO + 逻辑分析仪） | 2Mbps 端到端 <500us |
| 8 | 长时间测试（8 小时过夜） | <0.01% 丢包，零 panic |

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| 完全无法通信（步骤 2 失败） | 中 | 严重 | 每层添加诊断计数器；与正常工作的 Gazell 寄存器转储对比 |
| 包损坏（payload 不匹配） | 低 | 高 | 验证 CRC 配置；检查字节序（nRF52 小端）；用固定模式测试 |
| ACK 超时过于激进 | 中 | 中 | 从 120us 增加到 250us 如果失败 |
| 长时间测试暴露内存损坏 | 低 | 严重 | 启用 defmt；跟踪缓冲池在途计数；栈金丝雀 |

**回退方案**：对比 Gazell 与 nrf-esb 的 RADIO 寄存器转储。

---

### M10: MPSL 时隙适配器（5-7 天）— 硬需求

**文件**：`src/mpsl_timeslot.rs`

**状态**：这是硬需求，不是可选的。BLE+ESB 并发是 RMK 的核心用例。（feature = "mpsl"）

**参考资料**：
- `too1/ncs-esb-ble-mpsl-demo/timeslot_handler.c`
- `inductivekickback/ncs_ble_esb_demo/timeslot.c`
- `nrfconnect/sdk-nrf/samples/esb/esb_ptx_ble/`
- [MPSL Timeslot API 文档](https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/mpsl/doc/timeslot.html)

#### 参考分析的关键发现

1. **每次时隙之间循环 RADIO 电源**：`RADIO.POWER = Off; RADIO.POWER = On` 清除 BLE 状态
2. **每次时隙完全重新初始化 ESB**（非轻量级挂起）
3. **跨时隙 PID 持久化**：挂起前保存，恢复后还原
4. **MPSL API 序列化**：所有调用通过单一协作上下文
5. **TIMER0 被 MPSL 独占**：ESB 使用 TIMER1/2
6. **HFCLK**：`XTAL_GUARANTEED` 对键盘更安全
7. **ZLI 解决方案**：MPSL callback 在优先级 0（不可屏蔽）。不能调内核函数。通过 `Signal<RawMutex, _>` 通知 async 任务
8. **时隙扩展**：可在时隙内请求扩展

**RMK PRX 部署策略**（修复 C4）：
- **推荐方案**：Dongle（PRX）跑独占模式 — USB 专用，无需 BLE。Keyboard（PTX）跑 MPSL 时隙模式 — 需要 BLE + ESB 并发。
- **如果 dongle 也需 BLE**：需要 synchronized timeslot 或信标模式（PRX 和 PTX 在约定时间窗口通信）。这大幅增加复杂度，建议延后到 M10 之后作为独立里程碑。

**HW 验证**：
- BLE 广播 + ESB 数据包在时隙内成功
- 延迟对比：时隙模式 vs 独占模式
- 阻塞/取消时隙恢复

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| `nrf-mpsl` 未暴露时隙 API | 中 | 严重 | 检查绑定；自行添加 FFI |
| ZLI + Embassy async 冲突 | 高 | 高 | 使用 `Signal<RawMutex, TimeslotSignal>`（无锁）从 ZLI 通知。不要从 ZLI 调任何 Embassy API |
| 时隙太短 | 中 | 高 | TX ramp-up(140us) + payload(128us) + ACK wait(260us) ≈ 530us。5ms 时隙可做 9+ 事务 |
| PRX 在时隙中不可靠 | 高 | 高 | PRX 必须整个时隙监听。考虑信标模式 |

**回退方案**：Phase 5A 独占模式独立工作。Phase 7 可推迟。

---

### M11: RMK 集成（2-3 天）

**RMK 中的文件**：`rmk/src/split/esb.rs`（对应 `gazell.rs`）

- 为 `EsbPtx`/`EsbPrx` 实现 `SplitReader`/`SplitWriter` trait
- 在 `radio_dispatch.rs` 中添加 `RadioMode::Esb = 3`
- 用 `nrf-esb` 替换 `rmk-gazell-sys` 依赖
- 添加 `"esb"` 连接类型到 `rmk-macro` codegen
- ISR bridge：扩展动态 RADIO 分派（`AtomicU8` 选择器）以支持 ESB 模式

**风险分析**：
| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|----------|
| ISR bridge 与 BLE/Gazell 冲突 | 中 | 高 | 遵循 Phase 4 的避免 `bind_interrupts!` 模式 |
| 代码生成需要新连接类型 | 中 | 中 | 在 codegen 中 `"esb"` 与 `"gazell"` 并列 |
| Feature flag 隔离 | 低 | 中 | `wireless_esb` 与 `wireless_gazell` 互不干扰 |

**回退方案**：保留 `rmk-gazell-sys` 作为后备；ESB 在 feature gate `wireless_esb` 后面。

---

## 依赖关系图

```
M0 ─→ M1 ─→ M2 ─→ M3 ─→ M4 ─→ M5 ─→ M6 ─→ M7 ─→ M8 ─→ M8.5 ─→ M9 ─→ M10 ─→ M11
              └─────┘              └────────────┘
         (radio + timer       (PTX + PRX 状态机
          可并行)               可并行)
```

## 风险登记（全局）

| 风险 | 影响 | 概率 | 缓解措施 |
|------|------|------|----------|
| ISR 延迟导致丢失 radio 事件 | 严重 | 低 | RADIO/TIMER P0；ISR 最小化；单一 ISR 上下文（R9） |
| nrf-pac 0.3 静默 API 变化 | 中 | 中 | 逐个寄存器测试；与 Gazell 寄存器转储对比 |
| 缓冲池 DMA use-after-free | 严重 | 低 | AtomicU8 状态跟踪（R2） |
| 隐式寄存器默认值依赖 | 中 | 低 | 显式设置 SKIPADDR=1；文档化 S1INCL（R5, R16） |
| Errata [122] DMA 缓冲放置 | 严重 | 低 | 验证 PacketPool 在 RAM block 0（R7） |
| MPSL 时隙 API 在 Rust 中不可用 | 高 | 中 | Phase 5A 独立工作；Phase 7 可推迟 |

## 时间线

| 里程碑 | 工期（乐观） | 工期（现实） | 累计（现实） |
|--------|-------------|-------------|-------------|
| M0-M4: 核心基础设施 | 5-8 天 | 8-12 天 | 12 天 |
| M5-M7: 协议 + ISR | 5-7 天 | 6-8 天 | 20 天 |
| M8: Async API | 2-3 天 | 2-3 天 | 23 天 |
| M8.5: Suspend/Resume | 1-2 天 | 2-3 天 | 26 天 |
| M9: HW 验证（独占模式） | 3-5 天 | 5-10 天 | 36 天 |
| **Phase 5A 完成** | **~25 天** | **~30-36 天** | |
| M10: MPSL 时隙（硬需求） | 5-7 天 | 7-14 天 | 50 天 |
| M11: RMK 集成 | 2-3 天 | 2-3 天 | 53 天 |

> 注：硬件调试（M9）和 MPSL 集成（M10）是最大不确定性来源。如果独占模式一次通过，可能快很多；如果有隐蔽的时序 bug，可能远超估计。

## 验证检查清单（每个里程碑）

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --target thumbv7em-none-eabihf -- -D warnings`
- [ ] `cargo test`（主机端，适用的里程碑）
- [ ] `cargo check --target thumbv7em-none-eabihf`
- [ ] `cargo doc` 无警告
