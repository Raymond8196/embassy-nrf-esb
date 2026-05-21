# M9 预备笔记：代码走查 + 硬件验证 + 调试工具

照着做，按顺序来。每个 checkbox 完成后打勾。

---

## 第一部分：手动走查代码（2-3 天）

目标：不看代码能默写出 PTX 发包全路径。拿纸笔跟着走。

### 1.1 PTX 发包全路径

每一步写下三件事：(1) task 还是 ISR 上下文 (2) buffer slot 当前状态 (3) 出错会怎样

#### 阶段 1: 应用层入队 — task 上下文

```
isr.rs:176  ptx.send(b"hello")
│
├─ 检查 payload 长度 1..=252
│
├─ payload.rs:157  pool.alloc_tx()
│   遍历 N 个 slot，CAS: FREE → TX_QUEUED (Acquire)
│   ★ 如果全满 → 返回 Err(TxFull)，包被丢弃
│
├─ payload.rs:87   header_mut(idx): 写 length=5, no_ack=false
│
├─ payload.rs:67   buf_mut(idx): 复制 "hello" 到 offset 4
│   内存布局:
│   [rssi][pipe][length][pid_no_ack][h][e][l][l][o]
│    SW     SW    HW       HW        payload...
│    byte0  byte1 byte2    byte3     byte4+
│                  ↑
│                  DMA 从 byte2 开始 (DMA_OFFSET=2)
│
├─ payload.rs:176  pool.enqueue_tx(idx) → Channel::send().await
│   ★ 如果 channel 满会 yield（异步等待）
│
└─ state_machine.rs:417  trigger_send() → NVIC::pend(RADIO)
    为什么 pend 而不是直接调？因为状态机只能在 ISR 上下文跑。
    pend 触发 RADIO ISR，ISR 里去取包。
```

- [ ] 我能画出 Packet 的内存布局（4 字节 header + payload）
- [ ] 我理解 DMA_OFFSET=2 的原因（跳过 rssi/pipe 两个软件字段）
- [ ] 我理解为什么用 NVIC::pend 而不是直接调用状态机

#### 阶段 2: ISR 取包发送 — RADIO ISR 上下文

```
isr.rs:138  on_radio_interrupt() 被硬件触发
│
├─ timer_flag = AtomicBool.load(Acquire)
│   如果 true → 清除 (store false, Release)
│
├─ suppress = suspend_requested.load(Acquire)
│
├─ sm = UnsafeCell::get() → &mut PtxStateMachine
│   ★ 为什么用 UnsafeCell 不用 Mutex？
│     因为这个 struct 只在 ISR 上下文访问，不会并发。
│     Mutex 会引入 critical section 开销，ISR 里要极快。
│
└─ state_machine.rs:198  handle_radio_event(pool, timer_flag, suppress)
    │
    ├─ check_events(): 读 RADIO.EVENTS_DISABLED
    │   当前是第一次调用，state == Idle
    │   evts.disabled == false, evts.timer == false
    │   → evts.user_event() == true → 进入 Idle 分支
    │
    └─ send_next(pool)  (state_machine.rs:331)
        │
        ├─ tx_idx == NO_IDX（没有遗留包）
        │
        ├─ pool.try_dequeue_tx() → Channel::try_receive()
        │   取出之前 enqueue 的 idx
        │
        ├─ header_mut(idx).set_pid(self.pid)
        │   PID 是 2-bit (0-3)，用于接收端去重
        │
        ├─ pool.dma_ptr(idx) → buf 起始地址 + 2
        │
        ├─ pool.tx_to_dma(idx)
        │   状态变化: TX_QUEUED → IN_DMA
        │   compiler_fence(Release) + store(IN_DMA, Release)
        │
        └─ radio.rs:218  radio.transmit(pipe=0, dma_ptr, ack=true)
            │
            ├─ shorts: 加 DISABLED→RXEN（发完自动切 RX 等 ACK）
            ├─ INTENSET: 使能 DISABLED 中断
            ├─ TXADDRESS = 0, RXADDRESSES = 0x01
            ├─ PACKETPTR = dma_ptr
            ├─ 清 events (DISABLED, READY, END, PAYLOAD, ADDRESS)
            │   ★ 为什么在 TXEN 之前清？
            │     如果 event 已经 set，shortcut 会立即触发，
            │     导致状态机跳过步骤
            ├─ compiler_fence(Release) — buf 写入对 DMA 可见
            └─ TASKS_TXEN = 1 — 射频 ramp-up 开始
```

- [ ] 我理解 UnsafeCell 在这里的安全保证（单 ISR 上下文，不并发）
- [ ] 我理解 `user_event()` 的含义（!disabled && !timer，即软件触发的 pend）
- [ ] 我理解 DMA 前清 events 的原因
- [ ] 我能说出 tx_to_dma 里两个操作的顺序和为什么

#### 阶段 3: TX 完成，等 ACK — RADIO ISR 上下文

```
硬件序列: ramp-up → READY → START → 发包 → END → DISABLED
shortcut DISABLED→RXEN 使 RADIO 自动切到 RX
DISABLED 事件触发又一次 RADIO ISR

state_machine.rs:234  state == Tx, evts.disabled == true
│
├─ alloc_dma_buffer(pool)
│   找一个 FREE slot → CAS → IN_DMA
│   ★ 如果没有空闲 slot：
│     radio.stop(), state → Idle, tx_idx 保留
│     下次 send_next 会用这个 tx_idx 重发
│
├─ radio.rs:271  prepare_for_ack(ack_dma_ptr)
│   ├─ clear READY event
│   ├─ compiler_fence(Release)
│   ├─ PACKETPTR = ack buffer（切换到 ACK 接收 buffer）
│   ├─ debug_assert(!READY) ← 如果失败 = 太慢，错过 ACK 窗口
│   └─ 清 shortcut disabled_rxen（已经在 RX 了）
│
├─ timer.arm_retransmit(retransmit_delay - RAMP_UP)
│   CC[0] = 绝对值, tasks_clear + tasks_start（从 0 计时）
│   ★ 为什么减 RAMP_UP？
│     重发时 radio 要从 DISABLED re-enable，ramp-up 是额外时间，
│     delay 已经包含了这段时间，所以定时器要提前触发
│
└─ timer.arm_ack_timeout(ack_timeout + RAMP_UP)
    CC[1] = 相对值, tasks_capture(1) 读当前计数 + 加 timeout
    ★ 为什么加 RAMP_UP？
      等 ACK 时 radio 正在 ramp-up 到 RX，这段时间也要等，
      不能算作"超时"

state → WaitAck
```

- [ ] 我理解 ACK buffer alloc 失败时的处理（保留 tx_idx，下次重发）
- [ ] 我能解释 retransmit delay 减 ramp-up / ack timeout 加 ramp-up 的原因
- [ ] 我理解 CC[0] 绝对定时 vs CC[1] 相对定时的区别

#### 阶段 4a: ACK 收到 — RADIO ISR 上下文

```
state == WaitAck, evts.disabled == true（RX 完成）

├─ timer.disarm_ack_timeout()
│
├─ radio.check_ack() → CRCSTATUS == OK
│   compiler_fence(Acquire) ← DMA 写入的数据对 CPU 可见
│
│   CRC OK:
│   ├─ timer.disarm_retransmit()
│   ├─ radio.stop()
│   ├─ pool.release_rx(ack_rx_idx) → IN_DMA → FREE
│   ├─ pool.release_tx(tx_idx) → IN_DMA → FREE
│   ├─ attempts = 0
│   ├─ advance_pid(): pid = (pid + 1) & 0x03
│   └─ send_next(pool) → 发下一个包或 go idle
```

#### 阶段 4b: ACK 超时或 CRC 失败 → 重发

```
两种触发路径：
  (a) evts.disabled == true 但 CRC 失败
  (b) evts.timer == true（ACK 等待超时）

├─ radio.stop()
├─ pool.release_rx(ack_rx_idx) → FREE（释放 ACK buffer）
│
├─ attempts += 1
├─ if attempts >= max_attempts (注意 >= 不是 >):
│   ├─ pool.release_tx(tx_idx) → FREE（丢弃这个包）
│   ├─ max_attempts_flag = true
│   ├─ advance_pid()
│   └─ send_next 或 go idle
│   └─ return PtxEvent::MaxAttempts
│
├─ else: state → WaitRetransmit
│   等 retransmit timer CC[0] 触发
│
└─ WaitRetransmit + timer 事件:
    retransmit(pool) → 用同一个 tx_idx 和 dma_ptr 重发
    state → Tx（回到阶段 3）
```

- [ ] 我理解 `>=` 和 `>` 的区别（esb-ng 用 > 导致多一次重发，这里修正了）
- [ ] 我能说出一个包的完整 buffer 生命周期: FREE → TX_QUEUED → IN_DMA → FREE

### 1.2 PRX 收包路径（简要，自己对照 state_machine.rs:546-707 走一遍）

```
prx.start_listening()
  → alloc buffer (FREE → IN_DMA)
  → radio.start_receiving(pipes, dma_ptr)
  → TASKS_RXEN, shortcut DISABLED→TXEN

收到包 → DISABLED event → ISR
  → check_packet(): CRC OK?
    Bad CRC → stop, 重启 RX，同一个 buffer
    Good CRC → 读 RXMATCH (pipe), RXCRC, RSSISAMPLE
      → check_duplicate(pipe, pid, crc)
        重复 → send repeated ACK / 重启 RX
        新包 → update_detection, 写 header metadata
          → NoAck: rx_complete, 分配新 buffer, 重启 RX
          → NeedAck: setup_ack_tx, state → TxAck

TxAck 完成 → DISABLED event
  → rx_complete(pending_rx_idx) — 通知应用层
  → release ack buffer
  → 分配新 RX buffer, shortcut 切回 RX
```

- [ ] 我理解 PRX 的 duplicate detection: 同 pipe 的 CRC+PID 都相同 = 重复
- [ ] 我理解 fallback ACK: 没有 ACK payload 时发 2 字节 [0,0]
- [ ] 我走完了 PRX 路径

### 1.3 五个核心理解检查

不看代码，在纸上回答。答完再翻代码验证。

- [ ] **Q1: buffer 状态转换** — TX slot: FREE → TX_QUEUED → IN_DMA → FREE。谁做每一步？(task alloc, task enqueue, ISR tx_to_dma, ISR release_tx)
- [ ] **Q2: DMA 指针** — PACKETPTR 指向 buf[2]，跳过 rssi 和 pipe。硬件从 length 字段开始读/写。
- [ ] **Q3: 单 ISR 架构** — 所有状态机逻辑在 RADIO ISR。TIMER ISR 只做：清 event → 设 flag → pend RADIO。如果拆到两个 ISR 会有并发问题（两个 ISR 同时修改 state）。
- [ ] **Q4: compiler_fence 方向** — Release: 写完 buffer 后，启动 DMA 前。Acquire: DMA 完成后，读 buffer 前。漏 Release = RADIO 可能读到旧数据。
- [ ] **Q5: suspend 竞态** — try_suspend 失败 → 设 flag → disable IRQ 再查一次。不做第二次查 = ISR 可能在设 flag 前就完成了，signal 永远不 fire。

---

## 第二部分：添加调试 dump 工具（0.5 天）

### 2.1 radio.rs 新增 dump_regs

在 `src/radio.rs` 的 `impl EsbRadio` 里加：

```rust
/// Dump key RADIO registers via defmt.
/// Read-only, safe from any context.
#[cfg(feature = "defmt")]
pub(crate) fn dump_regs(&self) {
    let r = self.radio;
    defmt::info!("=== RADIO regs ===");
    defmt::info!("  STATE      = {:#010x}", r.state().read().0);
    defmt::info!("  FREQUENCY  = {}", r.frequency().read().frequency());
    defmt::info!("  PCNF0      = {:#010x}", r.pcnf0().read().0);
    defmt::info!("  PCNF1      = {:#010x}", r.pcnf1().read().0);
    defmt::info!("  CRCCNF     = {:#010x}", r.crccnf().read().0);
    defmt::info!("  CRCPOLY    = {:#010x}", r.crcpoly().read().0);
    defmt::info!("  CRCINIT    = {:#010x}", r.crcinit().read().0);
    defmt::info!("  BASE0      = {:#010x}", r.base0().read());
    defmt::info!("  BASE1      = {:#010x}", r.base1().read());
    defmt::info!("  PREFIX0    = {:#010x}", r.prefix0().read().0);
    defmt::info!("  PREFIX1    = {:#010x}", r.prefix1().read().0);
    defmt::info!("  SHORTS     = {:#010x}", r.shorts().read().0);
    defmt::info!("  INTENSET   = {:#010x}", r.intenset().read().0);
    defmt::info!("  PACKETPTR  = {:#010x}", r.packetptr().read());
    defmt::info!("  TXPOWER    = {:#010x}", r.txpower().read().0);
    defmt::info!("  CRCSTATUS  = {}", r.crcstatus().read().0);
    defmt::info!("  RXMATCH    = {}", r.rxmatch().read().rxmatch());
    defmt::info!("  RXCRC      = {:#06x}", r.rxcrc().read().rxcrc());
    defmt::info!("  RSSISAMPLE = {}", r.rssisample().read().rssisample());
}
```

关键新增：`r.state().read()` — RADIO 当前运行状态 (Disabled/RxRu/Rx/TxRu/Tx/...)。
排查"什么都没发生"时第一个看的寄存器。

### 2.2 payload.rs 新增 dump_state

在 `src/payload.rs` 的 `impl PacketPool` 里加：

```rust
/// Dump pool slot states via defmt.
#[cfg(feature = "defmt")]
pub fn dump_state(&self) {
    defmt::info!("=== PacketPool ===");
    for i in 0..N {
        let s = self.state[i].load(core::sync::atomic::Ordering::Relaxed);
        let name = match s {
            0 => "FREE",
            1 => "TX_QUEUED",
            2 => "IN_DMA",
            3 => "RX_QUEUED",
            _ => "???",
        };
        defmt::info!("  slot[{}] = {}", i, name);
    }
}
```

所有 slot 都卡在 IN_DMA = buffer leak，最常见的 bug 表现。

### 2.3 isr.rs 暴露公开 debug 方法

在 `EsbPtx` 和 `EsbPrx` 的 impl 里各加：

```rust
/// Dump RADIO registers (debug helper).
#[cfg(feature = "defmt")]
pub fn dump_radio_regs(&self) {
    let sm = unsafe { &*self.sm.get() };
    sm.radio.dump_regs();
}
```

### 2.4 加完后验证

```bash
cargo check --features nrf52840,defmt --target thumbv7em-none-eabihf
```

- [ ] dump_regs 加到 radio.rs
- [ ] dump_state 加到 payload.rs
- [ ] dump_radio_regs 暴露到 EsbPtx/EsbPrx
- [ ] 编译通过

---

## 第三部分：硬件验证步骤（7-10 天）

### 3.0 准备清单

- [ ] 两块 nRF52840 板：E104-BT5040U (PRX) + nice!nano (PTX)
- [ ] probe-rs 安装 OK (`probe-rs info` 能识别板子)
- [ ] 如有逻辑分析仪更好，不必须

### Step 0: 工具链验证（0.5 天）

目标：板子能烧录，能看到 defmt 输出。

```bash
cargo build --example ptx_basic --features nrf52840,defmt --target thumbv7em-none-eabihf
probe-rs run --chip nRF52840_xxAA target/thumbv7em-none-eabihf/debug/examples/ptx_basic
```

- [ ] PTX 板看到 `ESB init ok` + 寄存器 dump
- [ ] PRX 板看到 `ESB init ok` + 寄存器 dump

如果 nice!nano 的 SWD 不通：尝试焊 SWD pad，或改用 UF2 烧录（但会失去 defmt）。

### Step 1: 寄存器值对比（0.5 天）

目标：确认 init() 写入的值和 Nordic ESB 标准一致。

把 defmt 输出的寄存器值填入下表，逐个对比：

| 寄存器 | 你的值 | 期望值 | 对应代码 | 通过 |
|--------|--------|--------|----------|------|
| FREQUENCY | | 2 (=2402MHz) | radio.rs:144 | [ ] |
| MODE | | 0x01 (2Mbit) 或 0x00 (1Mbit) | radio.rs:71 | [ ] |
| PCNF0 | | LFLEN=8, S1LEN=3 | radio.rs:104-107 | [ ] |
| PCNF1 | | MAXLEN=252, BALEN=4, ENDIAN=BIG | radio.rs:112-117 | [ ] |
| CRCCNF | | LEN=TWO, SKIPADDR=? | radio.rs:131-134 | [ ] |
| CRCPOLY | | 0x00011021 | radio.rs:130 | [ ] |
| CRCINIT | | 0x0000FFFF | radio.rs:128-129 | [ ] |
| BASE0 | | bit-reversed [E7,E7,E7,E7] | radio.rs:137 | [ ] |
| PREFIX0 | | bytewise-swapped [E7,C2,C3,C4] | radio.rs:139 | [ ] |
| TXPOWER | | 0x00 (0 dBm) | radio.rs:98 | [ ] |

**重点检查**：CRCCNF 的 SKIPADDR 字段。
- 你的代码 (radio.rs:133): `Skipaddr::INCLUDE`
- Nordic ESB 标准: SKIPADDR = Skip（地址不参与 CRC）
- esb-ng 用的: 需要查证
- 如果两端 SKIPADDR 不一致 → CRC 永远校验失败 → 收不到任何包

### Step 2: 只发不收 — 确认射频有信号（1 天）

目标：排除"RADIO 根本没发包"的可能。

**方案 A（有第三块板或 nRF Sniffer）**：
用 Wireshark + nRF Sniffer 在 2402 MHz 抓包。

**方案 B（只有两块板，推荐）**：
写一个极简 raw RX 程序，不用你的 ESB crate，直接操作 PAC 寄存器。
这样如果收不到，问题一定在 PTX 端或配置上，不会是 PRX 代码 bug。

```rust
// 极简 raw RX 伪代码 — 只用 PAC，不用 ESB crate
let r = pac::RADIO;

// 配置和 PTX 完全一样的参数
r.frequency().write(|w| w.set_frequency(2));
r.mode().write(|w| w.set_mode(Mode::NRF_2MBIT));
// ... pcnf0, pcnf1, crccnf, crcpoly, crcinit, base0, prefix0 ...
// 照搬 ptx_basic 的 dump 值

r.rxaddresses().write_value(0x01); // pipe 0
r.packetptr().write_value(rx_buf.as_mut_ptr() as u32);

// 手动收一个包
r.tasks_rxen().write_value(1);
while r.events_end().read() == 0 {} // 轮询等
r.events_end().write_value(0);

// 打印结果
let crc_ok = r.crcstatus().read().crcstatus() == Crcstatus::CRCOK;
defmt::info!("CRC: {}, buf: {=[u8]:x}", crc_ok, &rx_buf[..16]);
```

- [ ] 方案 A 或 B 确认射频有包发出
- [ ] 如果用方案 B 且 CRC 失败：对比两端 CRCCNF/CRCPOLY/CRCINIT 配置

### Step 3: 单向通信 — PTX→PRX（1 天）

两端都用你的 ESB crate。

- PTX: 运行 `ptx_basic`，每 500ms 发 4 字节 counter
- PRX: 运行 `prx_basic`，打印收到的包

```
PTX 端应该看到: [TX] #0: sent ok
PRX 端应该看到: [RX] #1: pipe=0 len=4 data=00000000
```

**如果 PTX 一直 "max retransmit reached"，PRX 什么也没收到**：

排查清单（按顺序）：

1. [ ] 两端频率一样？(FREQUENCY 寄存器)
2. [ ] 两端地址一样？(BASE0, PREFIX0 寄存器)
3. [ ] 两端包格式一样？(PCNF0, PCNF1 寄存器)
4. [ ] 两端 CRC 配置一样？(CRCCNF, CRCPOLY, CRCINIT)
5. [ ] 特别是 SKIPADDR 一致？
6. [ ] PRX 端 `start_listening()` 调用了？
7. [ ] PRX 端 RADIO ISR handler 绑定了？
8. [ ] PTX 端调 `dump_radio_regs()`，看 STATE 寄存器 — 卡在哪个状态？
9. [ ] PRX 端调 `dump_radio_regs()`，看 STATE 寄存器
10. [ ] 调 `pool.dump_state()` — 有没有 buffer 全卡 IN_DMA？

### Step 4: 双向通信 — ACK with payload（1-2 天）

修改 prx_basic，收到包后发 ACK payload：

```rust
let pkt = prx.receive().await;
// 立即排队一个 ACK payload 给下一个包
let _ = prx.send_ack_payload(0, b"pong").await;
```

PTX 端检查：

```rust
if let Some(ack) = ptx.try_receive() {
    defmt::info!("ACK: {=[u8]:x}", ack.payload());
    // 应该看到 "pong" = [70, 6f, 6e, 67]
}
```

**如果 PTX 能发成功但收不到 ACK payload**：
- 可能 ACK timeout 太紧：把 `ack_timeout_us` 从 120 改到 250 试试
- 检查 PRX 端的 ACK TX shortcut (DISABLED→TXEN → DISABLED→RXEN 切换)
- PRX 端 `dump_radio_regs()` 看 SHORTS 值

- [ ] PTX 收到 ACK payload
- [ ] 空 ACK（不 send_ack_payload）也能正常工作（fallback [0,0]）

### Step 5: 压力测试（1-2 天）

修改 ptx_basic，去掉 500ms 延迟，加统计：

```rust
let mut sent: u32 = 0;
let mut acked: u32 = 0;
let mut dropped: u32 = 0;

loop {
    let payload = sent.to_le_bytes();
    match ptx.send(&payload).await {
        Ok(()) => sent += 1,
        Err(_) => {},
    }

    if ptx.max_attempts_reached() {
        dropped += 1;
    }

    if let Some(_ack) = ptx.try_receive() {
        acked += 1;
    }

    if sent % 100 == 0 {
        defmt::info!("sent={} acked={} dropped={}", sent, acked, dropped);
        // 加上 pool dump 检测泄漏
        POOL.dump_state();
    }

    embassy_time::Timer::after_millis(10).await; // 10ms 间隔
}
```

通过标准：
- [ ] 1000 包 @ 1m: 丢包 < 1%
- [ ] pool 的 FREE slot 数量稳定（不单调递减 = 无泄漏）
- [ ] 跑 1 小时无 panic

**如果丢包严重（>5%）**：
- 增大 retransmit.count: 3 → 10
- 增大 retransmit.delay_us: 500 → 1000
- 增大 ack_timeout_us: 120 → 250
- 降低发包速率: 10ms → 50ms

**如果 pool 泄漏（FREE 越来越少）**：
- 某个路径没有 release buffer
- 在 state_machine.rs 里搜所有 `release_tx` / `release_rx` 调用
- 检查 error/edge case 路径是否遗漏了 release

---

## 时间线总览

| 任务 | 天数 | 可并行 |
|------|------|--------|
| 1.1-1.3 代码走查 | 2-3 天 | — |
| 2.1-2.4 加 dump 工具 | 0.5 天 | 可和走查并行 |
| Step 0 工具链验证 | 0.5 天 | 走查完成后开始 |
| Step 1 寄存器对比 | 0.5 天 | |
| Step 2 只发不收 | 1 天 | |
| Step 3 单向通信 | 1 天 | |
| Step 4 双向 ACK | 1-2 天 | |
| Step 5 压力测试 | 1-2 天 | |

**总计: 7-10 天**

---

## 待查问题

在走查或硬件测试中如果发现以下问题，记录在这里：

### CRCCNF.SKIPADDR 配置确认

radio.rs:133 用了 `Skipaddr::INCLUDE`。需要确认：
- [ ] esb-ng 用的是 INCLUDE 还是 SKIP？
- [ ] Nordic ESB C SDK 用的是哪个？
- [ ] 如果不一致，PTX 和 PRX 之间 CRC 会不匹配

### 其他发现

（走查/测试过程中随时记录）

-
-
-
