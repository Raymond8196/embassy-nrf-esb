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
cargo check --example ptx_suspend --features nrf52840,defmt,_cs-cortex
```

Expected feature conflict:

```bash
cargo check --features nrf52840,mpsl,_cs-cortex
```

This must fail with the `mpsl` / `_cs-cortex` mutual-exclusion error.

## Current Host Coverage

As of 2026-05-20:

- `EsbHeader` PID and NoAck bit layout.
- `EsbHeader` DMA/payload offsets and struct layout.
- `EsbAddresses` pipe count validation and enabled mask.
- `EsbAddresses` prefix lookup bounds.
- `EsbAddresses` base and prefix register bit reversal/packing.
- `EsbConfig` default validity and validation boundaries.
- `PacketPool` TX allocation is not visible until enqueue.
- `PacketPool` pipe-filtered TX dequeue claims only the requested pipe.
- `PacketPool` cancel releases allocated and queued TX slots.
- Duplicate detection requires a valid bit before PID/CRC comparison.
- Duplicate detection PID/CRC/valid state saves and restores.

## Hardware Matrix

Use two nRF52840 boards unless noted otherwise. Record board model, firmware
commit, feature flags, RF channel, payload length, and power source for each run.

| ID | Test | Firmware pair | Duration | Pass criteria | Result |
|----|------|---------------|----------|---------------|--------|
| C1 | PTX/PRX basic | `ptx_basic` + `prx_basic` | 30 min | No panic; TX/RX counters progress; no stuck radio. | Pending |
| C2 | ACK payload echo | `ptx_ack_echo` + ACK-capable PRX | 30 min | ACK payload count progresses monotonically; no counter inversion. | Pending |
| C3 | Multi-pipe | `ptx_multipipe` + multi-pipe PRX | 30 min | pipe0/pipe1/pipe2 all ACK; no cross-pipe ACK payloads. | Pending |
| C4 | Suspend/resume | `ptx_suspend` + PRX | 10k cycles or 30 min | No deadlock; traffic resumes after restore; no false duplicate burst. | Pending |
| C5 | NoAck | NoAck PTX + PRX | 30 min | PRX receives NoAck packets; no ACK TX shortcut regression. | Pending |
| C6 | Long idle recovery | PRX idle/listen transitions | 30 min | Repeated `start_listening()`/`stop()` does not wedge RADIO. | Pending |

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

## Acceptance For 9/10 Core

Core ESB reaches the target when:

- The build gate is green.
- C1-C5 pass at least once on the current primary nRF52840 hardware.
- C1-C3 have at least one 30 minute clean run.
- Suspend/resume has either 10k successful cycles or a 30 minute clean run.
- No known host-testable protocol bug remains without a regression test.

