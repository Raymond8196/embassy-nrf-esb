# USB CDC 断连后无法恢复输出

## 状态：未解决

## 问题表现

1. 固件启动后 `cat /dev/ttyACM0` 能正常读取所有 batch 输出
2. `cat` 退出（Ctrl+C / timeout）后，再次 `cat /dev/ttyACM0` 拿不到任何数据
3. 需要 reset dongle 重刷固件才能恢复

## 环境

- `embassy-usb = "0.6"` / `embassy-nrf = "0.10"` (nrf52840)
- Rust 1.94.0 stable, target `thumbv7em-none-eabihf`
- nRF52840 dongle, DFU bootloader
- Linux 6.8, clang 14

## 根因

`embassy-usb` 的 `CdcAcmClass::write_packet(&mut self, data: &[u8]) -> Result<(), EndpointError>`
在主机不消费 USB IN endpoint 时会永久阻塞（await 永远不返回），没有超时或错误返回机制。

代码流程：

```rust
loop {
    let r = run_prx_slots(...).await;  // ~6 秒
    if class.dtr() {                   // DTR 检查
        for chunk in w.bytes().chunks(64) {
            let _ = class.write_packet(chunk).await;  // ← 可能永久阻塞
        }
    }
}
```

DTR 检查能处理"cat 在 `run_prx_slots()` 期间退出"的情况（DTR 变 false，跳过写入）。
但无法处理"cat 在 `write_packet()` 执行中退出"——此时已过 DTR 检查，`write_packet` 阻塞，
循环卡死，再也到不了下一次 DTR 检查。

## 尝试过的方案

### 1. `with_timeout(Duration::from_millis(500), write_packet())`

**结果**：固件启动后完全无输出（连 "PRX-in-slot ready" 都没有）。

**分析**：`with_timeout` 到期后 drop `write_packet` 的 future，USB IN endpoint 的 DMA 传输
被中途取消，endpoint 状态损坏，后续所有写入失败或阻塞。

### 2. `if !dtr() { wait_connection().await }`

**结果**：`wait_connection()` 立即返回，无效。

**分析**：`wait_connection()` 等待 USB 进入 Configured 状态。主机关闭串口后 USB 连接
仍然存在（物理 USB 未断开），设备仍处于 Configured 状态，所以 `wait_connection()` 立即
返回。它无法检测"串口应用关闭"这个事件。

### 3. DTR 检查 + 跳过写入

```rust
if class.dtr() {
    for chunk in w.bytes().chunks(64) {
        let _ = class.write_packet(chunk).await;
    }
}
```

**结果**：能防止 `run_prx_slots()` 期间断连导致的卡死，但断连后第二次 `cat` 连接仍无输出。

**分析**：DTR 检查正确跳过了无主机时的写入，但当第二次 `cat` 连接后 DTR 变 true，
`write_packet()` 仍然阻塞。原因可能是 USB IN endpoint 在上一次传输后处于某种
中间状态（NAK'd），即使新主机连接也无法恢复。

## 关键观察

- `write_packet` 对小于 64 字节的数据通常能一次成功
- 数据量超过 64 字节时会被 `chunks(64)` 分成多次调用，每次都是独立的 USB 传输
- 第一次 `cat` 能完整读出所有数据（包括多次 chunk 写入），说明写入本身没问题
- 问题只发生在"写入过程中主机消失"的场景

## 候选方案（待验证）

### A. 独立 USB writer task + channel

```rust
// 主循环不直接写 USB，通过 channel 发送
let (tx, rx) = embassy_sync::channel::Channel::new().split();
// 独立 task 消费 channel，write_packet 阻塞不影响主循环
embassy_executor::spawn!(usb_writer_task(class, rx));
```

主循环通过 channel 发送数据，USB writer task 消费。channel 满时丢弃数据，
不阻塞主循环。但 `write_packet` 阻塞时 writer task 也卡住，channel 积压。

### B. 调研 embassy-usb endpoint API

- 是否有 non-blocking write（try_write）？
- 是否有 endpoint reset / abort API？
- 是否能检测 USB suspend / resume？

### C. `select` + USB disconnect event

用 `embassy_futures::select::select` 同时等待 `write_packet` 和 USB disconnect 信号，
disconnect 时取消写入。需要确认 embassy-usb 是否暴露 disconnect future。

### D. 硬件 USB DISABLE → RE-ENABLE

在检测到 DTR 从 true → false 后，禁用再重新使能 USB 外设，强制 endpoint 状态重置。
侵入性大，可能影响整个 USB 栈。

## 影响范围

此问题仅影响 USB CDC 调试输出。生产环境不使用 USB CDC，或使用更健壮的写入策略，
因此**不影响核心 ESB/MPSL 功能**。
