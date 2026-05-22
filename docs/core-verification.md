# Core ESB Verification

Created: 2026-05-20

This document tracks verification for the non-MPSL ESB core. MPSL/BLE
coexistence remains in `docs/m10-verification.md`.

## Scope

Core verification covers:

- Exclusive PTX and PRX operation.
- ACK payloads.
- NoAck packets.
- Multi-pipe routing.
- Suspend/resume.
- Host-testable protocol and buffer invariants.

It does not cover:

- MPSL timeslots.
- nRF SDC BLE coexistence.
- RMK split end-to-end behavior.

## Build Gate

Run before any core behavior change:

```bash
cargo fmt --check
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
cargo check --features nrf52840,_cs-cortex
cargo check --example ptx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_usb --features nrf52840,_cs-cortex
cargo check --example ptx_silent --features nrf52840,_cs-cortex
cargo check --example usb_minimal --features nrf52840,_cs-cortex
cargo check --example ptx_ack_echo --features nrf52840,_cs-cortex
cargo check --example ptx_multipipe --features nrf52840,defmt,_cs-cortex
cargo check --example prx_multipipe_usb --features nrf52840,_cs-cortex
cargo check --example ptx_multipipe_ack --features nrf52840,_cs-cortex
cargo check --example ptx_suspend --features nrf52840,defmt,_cs-cortex
cargo check --example ptx_noack_usb --features nrf52840,_cs-cortex
cargo check --example prx_noack_usb --features nrf52840,_cs-cortex
cargo check --example ptx_suspend_usb --features nrf52840,_cs-cortex
cargo check --example prx_idle_usb --features nrf52840,_cs-cortex
```

Expected feature conflict:

```bash
cargo check --features nrf52840,mpsl,_cs-cortex
```

This must fail with the `mpsl` / `_cs-cortex` mutual-exclusion error.

## Current Host Coverage

As of 2026-05-21:

- `EsbHeader` PID and NoAck bit layout.
- `EsbHeader` DMA/payload offsets and struct layout.
- `EsbAddresses` pipe count validation and enabled mask.
- `EsbAddresses` prefix lookup bounds.
- `EsbAddresses` base and prefix register bit reversal/packing.
- `EsbConfig` default validity and validation boundaries.
- `PacketPool` TX allocation is not visible until enqueue.
- `PacketPool` pipe-filtered TX dequeue claims only the requested pipe.
- `PacketPool` cancel releases allocated and queued TX slots.
- `PacketPool` RX DMA claim rejects busy slots and preserves buffer contents.
- Duplicate detection requires a valid bit before PID/CRC comparison.
- Duplicate detection PID/CRC/valid state saves and restores.
- Transport frame encode/decode round trips header and payload.
- Transport frame rejects truncated buffers and unsupported versions.
- Transport frame sizing helpers include the transport header.
- `SequenceTracker` accepts new per-device sequences and rejects duplicates.
- `StaticBindingTable` validates pipe-to-device mappings and can be const-initialized.

## Hardware Matrix

Use two nRF52840 boards unless noted otherwise. Record board model, firmware
commit, feature flags, RF channel, payload length, and power source for each run.

| ID | Test | Firmware pair | Duration | Pass criteria | Result |
|----|------|---------------|----------|---------------|--------|
| C1 | PTX/PRX basic | `ptx_basic` + `prx_basic` | 30 min | No panic; TX/RX counters progress; no stuck radio. | Pending |
| C2 | ACK payload echo | `ptx_ack_echo` + ACK-capable PRX | 30 min | ACK payload count progresses monotonically; no counter inversion. | Passed on 2026-05-22 |
| C3 | Multi-pipe | `ptx_multipipe_ack` + `prx_multipipe_usb` | 30 min | pipe0/pipe1 both ACK; PRX counts stay balanced; `bad_pipe=0`, `malformed=0`, `invalid_ack=0`. | Passed on 2026-05-22 |
| C4 | Suspend/resume | `ptx_suspend_usb` + PRX | 10k cycles or 30 min | No deadlock; traffic resumes after restore; no false duplicate burst. | Passed on 2026-05-22 |
| C5 | NoAck | `ptx_noack_usb` + `prx_noack_usb` | 30 min | PRX receives NoAck packets; no ACK TX shortcut regression. | Passed on 2026-05-22 |
| C6 | Long idle recovery | `prx_idle_usb` + `ptx_noack_usb` | 30 min | Repeated `start_listening()`/`stop()` does not wedge RADIO. | Passed on 2026-05-22 |

## Data To Capture

For each hardware run, record:

- Commit hash.
- Example names and exact cargo feature flags.
- Board and memory layout file.
- RF channel and address configuration.
- Payload length.
- TX count.
- ACK count.
- ACK payload count.
- Max-attempt count.
- Duplicate count if available.
- Any panic/assert/timeout.
- USB CDC or RTT output excerpt.

## Hardware Results

### 2026-05-21 Exclusive ESB ACK Echo Smoke

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware pair: `prx_usb` on PRX, `ptx_ack_echo` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_ack_echo --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX USB CDC reached `rx=1200 lost=0 loss=0.0%` during a 12 second capture.
- PTX USB CDC reached `tx=1200 ack_rx=1199`; ACK payload data monotonically followed the counter.
- No panic, USB reset loop, or stalled counter was observed during the capture.

### 2026-05-22 Exclusive ESB ACK Echo Smoke

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware commit: `bf12546` plus local documentation/Makefile-only changes.
- Firmware pair: `prx_usb` on PRX, `ptx_ack_echo` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_ack_echo --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX USB CDC reached `rx=7600 lost=0 loss=0.0%` across repeated 12 second
  captures totaling roughly 72 seconds.
- PTX USB CDC reached `tx=7600 ack_rx=7599`; ACK payload data monotonically
  followed the counter.
- No panic, USB reset loop, or stalled counter was observed during the capture.

### 2026-05-22 Exclusive ESB ACK Echo 30 Minute Run

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware commit: `2505794`.
- Firmware pair: `prx_usb` on PRX, `ptx_ack_echo` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_ack_echo --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PTX USB CDC capture ran under `timeout 1800s` and exited by timeout after a
  clean 30 minute run.
- Final PTX excerpt reached `tx=181200 ack_rx=181198`; ACK payload data
  continued to follow the transmit counter monotonically through the full
  capture.
- A post-run PRX USB CDC sample reported `rx=2000 lost=0 loss=0.0%`.
- No panic, USB reset loop, or stalled counter was observed during the capture.

### 2026-05-22 Exclusive ESB Multi-Pipe Smoke

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware base commit: `2505794` plus local `ptx_multipipe_ack` and
  `prx_multipipe_usb` diagnostics.
- Firmware pair: `prx_multipipe_usb` on PRX, `ptx_multipipe_ack` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_multipipe_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_multipipe_ack --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX USB CDC reached `n=6200 p0=3100 p1=3100 bp=0 mf=0`; both pipes stayed
  balanced and no wrong-pipe or malformed packets were reported.
- PTX USB CDC reached `q0=3101 a0=3099 q1=3100 a1=3099 f=0 m=0 i=0`;
  both ACK payload counters progressed, with no TX pool saturation, max-attempt
  drops, or invalid ACK payloads.
- This is a 60 second smoke, not the 30 minute C3 acceptance run.

### 2026-05-22 Exclusive ESB Multi-Pipe 30 Minute Run

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware base commit: `2505794` plus local `ptx_multipipe_ack` and
  `prx_multipipe_usb` diagnostics.
- Firmware pair: `prx_multipipe_usb` on PRX, `ptx_multipipe_ack` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_multipipe_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_multipipe_ack --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX USB CDC capture ran under `timeout 1800s` and exited by timeout after a
  clean 30 minute run.
- Final PRX excerpt reached `n=180200 p0=90100 p1=90100 bp=0 mf=0`; both pipes
  stayed balanced and no wrong-pipe or malformed packets were reported.
- PTX USB CDC capture ran under `timeout 1800s` and exited by timeout after a
  clean 30 minute run.
- Final PTX excerpt reached `q0=90101 a0=90035 q1=90100 a1=90014 f=0 m=0 i=0`;
  both ACK payload counters progressed, with no TX pool saturation, max-attempt
  drops, or invalid ACK payloads. ACK payload counters lagged queued TX counts
  by 66 packets on pipe 0 and 86 packets on pipe 1 at the final sample.
- No panic, USB reset loop, or stalled counter was observed during the capture.

### 2026-05-22 Exclusive ESB NoAck 30 Minute Run

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware base commit: `f0919a1` plus local `ptx_noack_usb` and
  `prx_noack_usb` diagnostics later committed as `3f8ca84`.
- Firmware pair: `prx_noack_usb` on PRX, `ptx_noack_usb` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_noack_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_noack_usb --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX and PTX USB CDC captures ran under `timeout 1800s` and exited by timeout
  after a clean 30 minute run.
- Final PTX excerpt reached `n=180200 q=180201 f=0 m=0`; the TX queue kept
  progressing with no TX pool saturation or max-attempt reports.
- Final PRX excerpt reached `rx=180000 lost=177 mf=0`; NoAck traffic continued
  through the full run and no malformed packets were reported. The 177 counter
  gaps are expected RF loss for an unretried NoAck stream, not ACK failure.
- No panic, USB reset loop, or stalled counter was observed during the capture.

### 2026-05-22 Exclusive ESB Suspend/Resume 30 Minute Run

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware base commit: `f0919a1` plus local `ptx_suspend_usb` diagnostic
  later committed as `3f8ca84`.
- Firmware pair: `prx_usb` on PRX, `ptx_suspend_usb` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_suspend_usb --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX and PTX USB CDC captures ran under `timeout 1800s` and exited by timeout
  after a clean 30 minute run.
- Final PTX excerpt reached `c=1716 q=171600 a=171563 f=0 m=0`; 1,716
  suspend/restore cycles completed with queued traffic continuing after each
  restore.
- Final PRX excerpt reached `rx=171600 lost=0 loss=0.0%`.
- No panic, USB reset loop, TX pool saturation, max-attempt report, deadlock,
  or stalled counter was observed during the capture.

### 2026-05-22 Exclusive ESB PRX Idle/Listens 30 Minute Run

Environment:

- Hardware: two E104-BT5040U nRF52840 dongles.
- Memory layout: `memory-dongle.x`.
- Firmware commit: `3f8ca84` plus local documentation-only changes.
- Firmware pair: `prx_idle_usb` on PRX, `ptx_noack_usb` on PTX.
- Build target/features:
  - `cargo build --release --target thumbv7em-none-eabihf --example prx_idle_usb --features nrf52840,_cs-cortex`
  - `cargo build --release --target thumbv7em-none-eabihf --example ptx_noack_usb --features nrf52840,_cs-cortex`
- Flash path: unsigned serial DFU zips generated from Intel HEX and flashed
  over `/dev/ttyACM0` and `/dev/ttyACM1`.

Result:

- PRX and PTX USB CDC output was sampled after the run had exceeded the 30
  minute C6 threshold.
- Final PRX excerpt reached `c=1840 rx=173152 last=96 empty=26 mf=0`;
  repeated `start_listening()` / `stop()` cycles continued to recover and no
  malformed packets were reported.
- Final PTX excerpt reached `n=181800 q=181801 f=0 m=0`; the NoAck sender kept
  queueing packets without TX pool saturation or max-attempt reports.
- No panic, USB reset loop, wedged RADIO state, or stalled counter was observed
  during the capture.

## Acceptance For 9/10 Core

Core ESB reaches the target when:

- The build gate is green.
- C1-C5 pass at least once on the current primary nRF52840 hardware.
- C1-C3 have at least one 30 minute clean run.
- Suspend/resume has either 10k successful cycles or a 30 minute clean run.
- No known host-testable protocol bug remains without a regression test.
