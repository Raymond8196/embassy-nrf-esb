# M9: Hardware Verification — Exclusive Mode (Detailed)

## Hardware

- **PTX (键盘侧)**: nice!nano v2 (nRF52840)
- **PRX (Dongle 侧)**: E104-BT5040U (nRF52840)
- **调试**: probe-rs (`probe-rs run --chip nRF52840_xxAA`)
- **日志**: defmt RTT

## 前置工作：Example 基础设施

当前项目是纯 lib crate，缺少运行 example 所需的依赖。需要补充：

### Cargo.toml 新增 dev-dependencies

```toml
[dev-dependencies]
embassy-executor = { version = "0.8", features = ["arch-cortex-m", "executor-thread"] }
cortex-m-rt = "0.7"
defmt-rtt = "0.4"
panic-probe = { version = "0.3", features = ["print-defmt"] }
static_cell = "2"
```

### Example 文件结构

```
examples/
├── ptx_basic.rs       — Step 1-3: 基础 PTX 发送 + ACK
├── prx_basic.rs       — Step 1-3: 基础 PRX 接收 + ACK 响应
├── ptx_stress.rs      — Step 4-5: 千包流 + 重传测试
├── prx_stress.rs      — Step 4-5: 千包流 PRX 侧
└── common.rs          — 共享地址/配置常量
```

---

## 验证步骤

### Step 0: Example 骨架能跑 (0.5 天)

**目标**: 两块板子都能烧录、RTT 日志能看到。

**做什么**:
1. 补充 `Cargo.toml` dev-dependencies
2. 写最小 `examples/ptx_basic.rs`：init radio + defmt 打印配置
3. 写最小 `examples/prx_basic.rs`：init radio + defmt 打印配置
4. 分别烧录两块板，确认 RTT 输出

**通过标准**: 两块板 RTT 输出 "ESB init ok, channel=2, bitrate=2Mbps"

**烧录命令**:
```bash
# PTX (nice!nano)
cargo run --example ptx_basic --features nrf52840,defmt

# PRX (E104-BT5040U) — 可能需要不同 probe 序列号
cargo run --example prx_basic --features nrf52840,defmt -- --probe <SERIAL>
```

**风险**: probe-rs 连接问题、nice!nano 的 bootloader 可能需要 UF2 而非 SWD。如果 SWD 不通，改用 `probe-rs download` + UF2 回退方案。

---

### Step 1: RADIO 寄存器验证 (0.5 天)

**目标**: 确认所有 RADIO 寄存器写入正确。

**做什么**:
在 `ptx_basic.rs` init 后，用 defmt 读回关键寄存器并打印：
- `FREQUENCY` — 应为 2 (2402 MHz)
- `MODE` — 应为 NRF_2MBIT
- `PCNF0` — LFLEN=8, S1LEN=3
- `PCNF1` — MAXLEN=252, BALEN=4, ENDIAN=Big
- `CRCCNF` — LEN=TWO, SKIPADDR=INCLUDE
- `BASE0`, `PREFIX0` — 地址位反转值
- `TXPOWER` — 0 dBm

**通过标准**: 所有寄存器值与 `radio.rs::init()` 写入一致。

**排错**: 如果值不对，对比 esb-ng 的寄存器 dump。PAC 0.3 的写法可能有字段映射差异。

---

### Step 2: 单包 PTX→PRX (1 天)

**目标**: 一个包从 PTX 到达 PRX，payload 完整。

**做什么**:
1. PRX: `start_listening()`, 循环 `receive().await`
2. PTX: 发送固定 payload `[0xAA, 0x55, 0x01, 0x02]`
3. PRX: 打印收到的 pipe、length、payload 每字节

**通过标准**: PRX 收到 `[0xAA, 0x55, 0x01, 0x02]`，pipe=0，length=4。

**如果失败 — 分层排查**:
1. **PTX 侧确认 TX 事件**: 打印 `EVENTS_DISABLED` 是否触发、state machine 状态
2. **PRX 侧确认 RX 事件**: 打印 `EVENTS_DISABLED`、`CRCSTATUS`、`RXMATCH`
3. **CRC 不匹配**: 检查 `CRCCNF.SKIPADDR`、CRC poly/init 两侧是否一致
4. **完全没收到**: 检查频率、地址、bitrate 是否两侧匹配
5. **DMA 问题**: 确认 buffer 地址在 RAM block 0 (`< 0x20030000`)

---

### Step 3: ACK 往返 (0.5 天)

**目标**: PTX 发包后收到 PRX 的 ACK payload。

**做什么**:
1. PRX: 收到包后调用 `send_ack_payload(0, &[0xBB, 0xCC])`
2. PTX: 发包后 `receive().await` 检查 ACK payload

**通过标准**: PTX 收到 ACK payload `[0xBB, 0xCC]`。

**如果失败**:
- PTX 侧打印 `check_ack()` 的 CRC 结果
- 检查 ACK timeout 配置（默认 120us 可能太紧，先放宽到 250us）
- 确认 PRX 的 `DISABLED→TXEN` shortcut 正常（ACK 自动发送）

---

### Step 4: 千包流测试 (1 天)

**目标**: 1000 包连续发送，统计丢包率。

**做什么**:
1. PTX: 循环发送 1000 包，payload = 4 字节递增计数器 `[i as u32]`
2. PRX: 接收并校验计数器连续性，统计：收到数、丢失数、重复数、乱序数
3. 每 100 包 defmt 打印进度

**通过标准**:
- 1m 距离: 0% 丢包
- 5m 距离: < 1% 丢包
- 0 重复（PID duplicate detection 正常）

**如果丢包严重**:
- 增大 `retransmit.count`（默认 3 → 尝试 10）
- 增大 `retransmit.delay_us`（默认 500 → 尝试 1000）
- 检查 `max_attempts_reached()` 计数
- 降低发包速率（每包间加 1ms delay）

---

### Step 5: 重传与 MaxAttempts (0.5 天)

**目标**: 验证 PRX 离线时 PTX 正确报告 MaxAttempts，PRX 恢复后通信恢复。

**做什么**:
1. PTX 持续发送
2. 拔掉 PRX 电源 → PTX 应报告 `max_attempts_reached() == true`
3. 重新插上 PRX → PTX 应恢复正常发送

**通过标准**: MaxAttempts 在 PRX 断电后 < 100ms 内报告；恢复后下一包成功。

---

### Step 6: 多 Pipe (0.5 天)

**目标**: Pipe 0 和 Pipe 1 都能正确收发。

**做什么**:
1. 配置 `EsbAddresses` 启用 2 个 pipe（不同 prefix）
2. PTX 交替在 pipe 0 和 pipe 1 发送
3. PRX 校验 `ReceivedPacket::pipe()` 是否正确

**通过标准**: 两个 pipe 都收到正确 payload，pipe 号匹配。

---

### Step 7: Suspend/Resume 验证 (1 天)

**目标**: Suspend + restore 后通信恢复，PID 连续性不中断。

**做什么**:
1. PTX 发 100 包 → suspend → (等 500ms) → restore → 再发 100 包
2. PRX 统计 200 包的计数器连续性和重复数
3. 重复 10 次 suspend/resume 循环

**通过标准**: 200 包 0 丢包、0 重复。PID 在 suspend 前后连续（PRX 不拒绝为 duplicate）。

---

### Step 8: 长时间稳定性 (过夜跑)

**目标**: 8 小时无 panic、无内存泄漏。

**做什么**:
1. PTX: 10ms 间隔持续发包（约 288 万包/8h）
2. PRX: 接收并每 10000 包打印统计
3. defmt 打印 pool 状态（free buffer 数量），确认无泄漏

**通过标准**:
- < 0.01% 丢包
- 0 panic
- free buffer 数量稳定（不单调递减）

---

## 时间估算

| 步骤 | 天数 | 累计 |
|------|------|------|
| Step 0: Example 骨架 | 0.5 | 0.5 |
| Step 1: 寄存器验证 | 0.5 | 1 |
| Step 2: 单包通信 | 1 | 2 |
| Step 3: ACK 往返 | 0.5 | 2.5 |
| Step 4: 千包流 | 1 | 3.5 |
| Step 5: 重传测试 | 0.5 | 4 |
| Step 6: 多 Pipe | 0.5 | 4.5 |
| Step 7: Suspend/Resume | 1 | 5.5 |
| Step 8: 过夜稳定性 | 1 | 6.5 |

**乐观**: 5 天（前几步顺利合并）
**悲观**: 10 天（Step 2 卡寄存器调试 + 配置 timing 问题）
**最可能**: 7 天

## 预期卡点

| 卡点 | 可能性 | 应对 |
|------|--------|------|
| nice!nano 不支持 SWD 直连 | 中 | 用 UF2 bootloader 烧录，或焊 SWD pad |
| Step 2 完全没通信 | 中 | 逐层 dump：TX event → RX event → CRC → payload。对比 Gazell 寄存器 |
| ACK timing 过紧 | 中 | 放宽 ack_timeout 到 250us，retransmit_delay 到 1000us |
| Pool buffer 泄漏 | 低 | defmt 打 pool 状态；已经过 5 轮 review 修 buffer leak |
| defmt 影响 timing | 低 | 在 ISR 里不打 defmt；只在 app context 打 |
