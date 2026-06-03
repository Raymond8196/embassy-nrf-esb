# M10: MPSL Timeslot Adapter Verification

Created: 2026-05-19

This document tracks M10 verification separately from the implementation plan in `docs/m10-plan.md`. The top-level status below reflects the latest recorded local compile checks and two-dongle hardware runs from 2026-05-20.

## Current Status

| Step | Scope | Status | Notes |
|------|-------|--------|-------|
| 0 | MPSL init smoke | Build-verified | Hardware result exists from earlier bring-up but was not re-run after the latest stopgap fixes. |
| 1 | Single timeslot request | Build-verified | `mpsl_request_basic` compile check passes; latest hardware re-run not recorded. |
| 2 | Chained timeslots | Build-verified | `mpsl_request_chained` compile check passes; latest hardware re-run not recorded. |
| 3-4 | PTX-in-timeslot + PID continuity | Hardware-smoke passed | `mpsl_ptx_in_slot` participates in the recorded two-dongle runs. |
| 5-6 | PRX-in-timeslot + multi-pipe ACK payload | Hardware-smoke passed | `mpsl_prx_in_slot` + `mpsl_ptx_in_slot` reached pipe0/pipe1 `tx=450 ack=450 ackpl=450`. |
| 7 | BLE coexistence first pass | Functional, needs tuning | `mpsl_prx_ble` can connect and stay connected; relaxed connection parameters restore useful ESB ACK coverage, but pipe1 remains below advertising-only throughput. |
| 8 | RMK-style split E2E | Not started | Requires RMK transport adapter and keyboard-style traffic. |
| 9 | Overnight/stress validation | Not started | Requires stable Phase 7 parameters and repeatable hardware setup. |

Summary:

- Passed: MPSL diagnostic compile checks, stopgap correctness checks, PRX/PTX multi-pipe ACK payload hardware smoke without active BLE connection.
- Functional but still below target: active BLE connection plus ESB PRX timeslots.
- Not started: RMK split end-to-end and overnight/stress validation.

## Build Matrix

Run these without hardware before every M10 change that touches Cargo features, MPSL, or examples.

Host-side unit tests:

```bash
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
```

The host test still enables `nrf52840` because `embassy-nrf`/`nrf-pac` require a concrete chip feature even when the tested modules are PAC-free.

```bash
cargo check --example ptx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_usb --features nrf52840,_cs-cortex
cargo check --example ptx_silent --features nrf52840,_cs-cortex
cargo check --example usb_minimal --features nrf52840,_cs-cortex
cargo check --example ptx_ack_echo --features nrf52840,_cs-cortex
cargo check --example ptx_multipipe --features nrf52840,defmt,_cs-cortex
cargo check --example ptx_suspend --features nrf52840,defmt,_cs-cortex

cargo check --example mpsl_smoke --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_basic --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_chained --features nrf52840,defmt,mpsl
cargo check --example mpsl_ptx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_ble --features nrf52840,defmt,mpsl
```

Intentional failure check:

```bash
cargo check --features nrf52840,mpsl,_cs-cortex
```

Expected result: compile fails with `features `mpsl` and `_cs-cortex` are mutually exclusive`.

## Compile-only Results

Date: 2026-05-19

Environment: local Linux workspace, no dongles attached.

Commands already checked successfully in this pass:

```bash
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
cargo check --example mpsl_prx_ble --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_ptx_in_slot --features nrf52840,defmt,mpsl
cargo check --example ptx_basic --features nrf52840,defmt,_cs-cortex
```

Current host-side coverage:

- `EsbHeader` PID and NO_ACK bit layout.
- `EsbHeader` DMA/payload offsets and struct layout.
- `EsbAddresses` pipe count validation and enabled mask.
- `EsbAddresses` prefix lookup bounds.
- `EsbAddresses` base and prefix register bit reversal/packing.
- `EsbConfig` default validity and validation boundaries.

### Stopgap Correctness Update

Date: 2026-05-20

Compile-only checks completed after the MPSL stopgap fixes:

```bash
cargo check --example mpsl_ptx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_ble --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_basic --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_chained --features nrf52840,defmt,mpsl
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
rustfmt --edition 2024 --check src/mpsl_timeslot.rs examples/mpsl_request_basic.rs examples/mpsl_request_chained.rs examples/mpsl_ptx_in_slot.rs examples/mpsl_prx_in_slot.rs examples/mpsl_prx_ble.rs
```

Notes:

- `OVERSTAYED` no longer panics in the generic, PTX, or PRX timeslot callbacks. It increments the existing counter, marks the session done, wakes the waiter, and asks MPSL to end the slot.
- PRX duplicate-detection state is no longer overwritten by a freshly-created `EsbRadio` at TIMER0 slot end. The manual PRX path keeps PID/CRC state in `PrxInnerState`.
- MPSL PTX and ACK payload construction now uses `EsbHeader::set_pid()` / `set_no_ack()` through a shared counter-packet helper.
- MPSL free functions now reject re-entry with `Error::Busy` before opening a second session on the same static state.
- Full `cargo fmt --check` still reports unrelated pre-existing formatting changes outside `src/mpsl_timeslot.rs`.

## Step 7 Hardware Procedure

When two dongles are available:

1. Flash `mpsl_prx_ble` on the PRX/main-side dongle.
2. Use nRF Connect mobile app to scan for the `ESB M10` advertiser.
3. Flash `mpsl_ptx_in_slot` on the second dongle.
4. Open the PTX USB CDC serial port and verify ACK counters progress while `ESB M10` remains visible.
5. Record PTX output lines and any BLE visibility issues here.

Pass criteria for the current first pass:

- `ESB M10` is visible in nRF Connect for at least 60 seconds.
- PTX reports nonzero `ack` and `ackpl` counts against the PRX running `mpsl_prx_ble`.
- No panic, MPSL assert, or overstay reset during a 5 minute smoke run.

## Hardware Results

Date: 2026-05-19

Hardware: two E104-BT5040U nRF52840 dongles, serial DFU via `nrfutil`.

Flashing notes:

- Built `mpsl_prx_ble` and `mpsl_ptx_in_slot` with `nrf52840,defmt,mpsl`.
- Converted ELF outputs to Intel HEX with `arm-none-eabi-objcopy`.
- Packaged unsigned app-only DFU zips with `nrfutil pkg generate --hw-version 52 --sd-req 0x00`.
- Flashed over `/dev/ttyACM*` using `nrfutil dfu serial --baud-rate 115200 --flow-control 0`.

Step 7 observations:

- Advertising-only diagnostic build of `mpsl_prx_ble` was visible as `ESB M10` in nRF Connect.
- Full `mpsl_prx_ble` with PRX timeslot session also remained visible as `ESB M10`.
- `mpsl_ptx_in_slot` USB CDC output against full `mpsl_prx_ble`:

```text
pipe=0 tx=418 ack=414 ackpl=414 ctr=1 inv=0 blk=0 can=0
pipe=1 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
DONE
```

Result:

- BLE advertising + ESB PRX timeslot coexistence is partially validated.
- Pipe 0 ACK payload works while BLE advertising remains visible.
- Pipe 1 failed in this run, so multi-pipe Step 7 is not passed yet.

Follow-up:

- Re-run PTX/PRX with pipe 0 only to establish a clean coexistence baseline.
- Investigate why pipe 1 gets `ack=0` against `mpsl_prx_ble`; compare with prior `mpsl_prx_in_slot` Step 6 behavior.
- Keep BLE advertising visible during the next PTX run to confirm coexistence over a longer window.

Follow-up run:

- Re-ran `mpsl_prx_in_slot` without BLE against `mpsl_ptx_in_slot`.
- Result matched the BLE run: pipe 0 ACKs, pipe 1 gets no ACK.

```text
pipe=0 tx=450 ack=450 ackpl=450 ctr=1 inv=0 blk=0 can=0
pipe=1 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
DONE
```

- Conclusion: pipe 1 failure is not caused by BLE coexistence. It is in the MPSL PRX/PTX multi-pipe path.
- Experimental manual-ACK PRX path, where software waits for RXMATCH before starting ACK TX, made pipe 1 ACKs appear but regressed pipe 0/burst reliability:

```text
pipe=0 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
pipe=1 tx=186 ack=153 ackpl=153 ctr=153 inv=0 blk=0 can=0
DONE
```

and with RXADDRESSES clearing before ACK TX:

```text
pipe=0 tx=150 ack=100 ackpl=100 ctr=149 inv=0 blk=0 can=0
pipe=1 tx=100 ack=50 ackpl=50 ctr=50 inv=0 blk=0 can=0
DONE
```

- Working hypothesis: PRX ACK TXADDRESS timing is wrong in the current hardware-auto-ACK MPSL path. For multi-pipe ACK, software must know RXMATCH before selecting TXADDRESS, but the `disabled_txen` shortcut can start TX before software updates it. The manual-ACK experiment supports this but needs a cleaner implementation/timing model before landing.

Clean manual-ACK implementation attempt:

- Added a PRX path that disables hardware RX->TX auto-ACK, reads RXMATCH, prepares ACK payload, sets TXADDRESS, then manually starts ACK TX.
- Burst run result:

```text
pipe=0 tx=450 ack=400 ackpl=400 ctr=449 inv=0 blk=0 can=0
pipe=1 tx=394 ack=344 ackpl=344 ctr=344 inv=0 blk=0 can=0
DONE
```

- This confirms both pipe 0 and pipe 1 can ACK with software-selected TXADDRESS, but it still drops one slot worth of packets per phase.
- Changing PTX to one packet per slot produced no ACKs in that setup, so the issue is not solved by lowering in-slot packet density alone.
- Trying an ADDRESS-event preselect approach, where hardware auto-ACK remains enabled and software writes TXADDRESS on RADIO ADDRESS, did not improve pipe 1:

```text
pipe=0 tx=450 ack=450 ackpl=450 ctr=1 inv=0 blk=0 can=0
pipe=1 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
DONE
```

- Reducing the manual-ACK burst to 9 packets per slot also failed in this setup:

```text
pipe=0 tx=100 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
pipe=1 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
DONE
```

- Increasing both PTX and PRX timeslots to 12 ms with an 11.5 ms in-slot match improved the manual-ACK path substantially:

```text
pipe=0 tx=450 ack=450 ackpl=450 ctr=450 inv=0 blk=0 can=0
pipe=1 tx=444 ack=442 ackpl=442 ctr=442 inv=0 blk=0 can=0
DONE
```

- This strongly suggests the manual-ACK approach is correct, but the old 9 ms slot is too tight for dense 10-packet bursts. The examples currently use 14 ms slots as a pending-validation smoke parameter while the MPSL PRX timing is being tuned.

- Keep the manual-ACK code only if the next iteration can explain and fix the slot-boundary loss. Otherwise prefer a smaller targeted fix or revert before merging.

Advertising baseline re-run:

- Tried a connectable-advertising variant locally (`ADV_IND` + SDC peripheral support), but `ESB M10` was not visible in nRF Connect after flashing. Reverted to the advertising-only baseline.
- Reflashed `mpsl_prx_ble` advertising-only baseline and confirmed `ESB M10` is visible in nRF Connect.
- Reflashed `mpsl_ptx_in_slot` and captured USB CDC output against `mpsl_prx_ble`:

```text
pipe=0 tx=294 ack=271 ackpl=271 ctr=271 inv=0 blk=0 can=0
pipe=1 tx=359 ack=341 ackpl=341 ctr=342 inv=0 blk=0 can=0
DONE
```

- Result: advertising-only BLE coexistence is visible and both ESB pipes ACK with payloads. Throughput is lower than the prior 12 ms manual-ACK tuning run, but pipe 1 no longer fails completely.

BLE-only connectable diagnostic:

- Added `mpsl_ble_connectable`, a diagnostic example that runs MPSL + nrf-sdc connectable advertising without ESB timeslots.
- Initial `ADV_IND` + SDC peripheral support advertised as `ESB CONN` but did not stay connected from nRF Connect.
- Adding `peripheral_count(1)` and minimal ATT/SMP/L2CAP handlers made the device connect, but nRF Connect repeatedly issued Read By Group Type requests until ATT discovery responses were completed.
- After adding Generic Access service discovery and Device Name characteristic responses, `ESB CONN` connected and stayed connected in nRF Connect.
- Conclusion: SDC connectable advertising requires at least a small host-side ATT/L2CAP responder for the nRF Connect smoke test. The next Step 7 increment is to merge this minimal responder into `mpsl_prx_ble` before reintroducing ESB PRX timeslots under a BLE connection.

Connectable PRX + ESB timeslot combined run:

- Merged the minimal responder into `mpsl_prx_ble`: `ADV_IND`, SDC peripheral support, `peripheral_count(1)`, event masks, and minimal ATT/SMP/L2CAP handling for Generic Access + Device Name discovery.
- Flashed `mpsl_prx_ble` and `mpsl_ptx_in_slot`.
- nRF Connect connected to `ESB M10` and stayed connected.
- `mpsl_ptx_in_slot` USB CDC output while BLE stayed connected:

```text
pipe=0 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
pipe=1 tx=67 ack=15 ackpl=15 ctr=29 inv=0 blk=0 can=0
DONE
```

Stopgap hardware smoke:

- Date: 2026-05-20
- Built and flashed `mpsl_prx_ble_20260520_stopgap.zip` to `/dev/ttyACM0` while in Open DFU Bootloader.
- Built and flashed `mpsl_ptx_in_slot_20260520_stopgap.zip` to `/dev/ttyACM1` while in Open DFU Bootloader.
- PTX USB CDC re-enumerated as `/dev/ttyACM0`.
- PTX output:

```text
connected
before slots
pipe=0 tx=354 ack=336 ackpl=336 ctr=343 inv=0 start=50 t0=50 radio=690 idle=1 blk=0 can=0
pipe=1 tx=383 ack=366 ackpl=366 ctr=373 inv=0 start=50 t0=50 radio=750 idle=1 blk=0 can=0
DONE
```

- Result: both pipes ACK with ACK payloads after the stopgap fixes; no blocked/cancelled signals were reported by PTX.
- Manual BLE observation: `ESB M10` was visible in nRF Connect and stayed connected during this firmware run.

- Result: BLE connection stability first pass is achieved, but ESB PRX timeslot throughput regressed severely under an active BLE connection. This is expected to need timeslot duty-cycle tuning or connection interval/latency changes before Step 7 passes the ESB receive-rate target.

Connection-parameter tuning run:

- Added HCI LE Connection Update request after BLE connection complete, asking for CI=100 ms, latency=4, supervision timeout=6 s.
- Reflashed `mpsl_prx_ble` and `mpsl_ptx_in_slot`.
- nRF Connect connected to `ESB M10` and stayed connected.
- `mpsl_ptx_in_slot` USB CDC output while BLE stayed connected:

```text
pipe=0 tx=446 ack=444 ackpl=444 ctr=444 inv=0 blk=0 can=0
pipe=1 tx=258 ack=233 ackpl=233 ctr=234 inv=0 blk=0 can=0
DONE
```

- Result: relaxing BLE connection parameters restores useful ESB ACK coverage under an active BLE connection. Pipe 0 is near baseline; pipe 1 still loses more packets than advertising-only and needs further scheduling/slot tuning.

Batch 4 protocol-correctness regression:

- Built `mpsl_prx_in_slot` and `mpsl_ptx_in_slot` with `nrf52840,defmt,mpsl`.
- Converted ELF outputs to Intel HEX with `arm-none-eabi-objcopy`.
- Packaged unsigned app-only DFU zips with `nrfutil pkg generate --hw-version 52 --sd-req 0x00`.
- Flashed PRX to `/dev/ttyACM0`, then PTX to `/dev/ttyACM1`.
- PTX USB CDC re-enumerated as `/dev/ttyACM0`.
- PTX output:

```text
connected
before slots
pipe=0 tx=450 ack=450 ackpl=450 ctr=450 inv=0 start=50 t0=50 radio=900 idle=1 blk=0 can=0
pipe=1 tx=450 ack=450 ackpl=450 ctr=450 inv=0 start=50 t0=50 radio=900 idle=1 blk=0 can=0
DONE
```

- Result: MPSL PRX/PTX multi-pipe ACK payload path passed for pipe 0 and pipe 1. Previous pipe 1 ACK failure is fixed in this diagnostic run, and ACK payload counters stayed monotonic per pipe.

Follow-up run, 2026-05-21:

- Rebuilt and flashed `mpsl_prx_in_slot` to `/dev/ttyACM0`, then
  `mpsl_ptx_in_slot` to `/dev/ttyACM1` while both boards were in Open DFU
  Bootloader.
- PTX USB CDC re-enumerated as `/dev/ttyACM0`.
- PTX output:

```text
connected
before slots
pipe=0 tx=450 ack=450 ackpl=450 ctr=450 inv=0 start=50 t0=50 radio=900 idle=1 blk=0 can=0
pipe=1 tx=445 ack=444 ackpl=444 ctr=444 inv=0 start=50 t0=50 radio=889 idle=1 blk=0 can=0
DONE
```

- Result: Both pipes produced ACK payloads with monotonic counters and no
  inversions. Pipe 0 was full coverage; pipe 1 dropped a small number of
  packets in this run, so keep this as a useful-but-not-perfect MPSL multi-pipe
  smoke result rather than a full 450/450 repeat.

Repeat run, 2026-05-21, same 10 packets-per-slot PTX build:

```text
connected
before slots
pipe=0 tx=450 ack=450 ackpl=450 ctr=450 inv=0 start=50 t0=50 radio=900 idle=1 blk=0 can=0
pipe=1 tx=450 ack=447 ackpl=447 ctr=450 inv=0 start=50 t0=50 radio=900 idle=1 blk=0 can=0
DONE
```

- Result: pipe 1 improved compared with the prior run but still missed a few
  ACKs. Because `tx=450` and `radio=900`, the PTX side completed the expected
  number of radio transitions; the remaining misses are consistent with
  PRX/PTX timeslot phase/timing misses rather than packet construction or
  static pipe routing failure.

Diagnostic 8 packets-per-slot PTX run, 2026-05-21:

```text
connected
before slots
pipe=0 tx=338 ack=334 ackpl=334 ctr=336 inv=0 start=50 t0=50 radio=673 idle=1 blk=0 can=0
pipe=1 tx=350 ack=349 ackpl=349 ctr=349 inv=0 start=50 t0=50 radio=699 idle=1 blk=0 can=0
DONE
```

- Result: reducing the per-slot packet target did not make the run full
  coverage. It instead showed that when an early packet in a slot misses ACK,
  the diagnostic PTX path can spend the rest of that slot waiting for radio
  completion until TIMER0 cuts the slot, reducing `tx` below the nominal
  `50 * packets_per_slot`. This points to missing ACK timeout/retry machinery in
  the MPSL diagnostic PTX path, not to the exclusive ESB core.

## 3-Mode Poll Diagnostic

Status date: 2026-06-01.

This section tracks the newer RMK-style 3-mode diagnostic pair:
`mpsl_3mode_central` on the PRX/main side and `mpsl_3mode_poll` on the PTX
poller side. It is separate from the older `mpsl_prx_ble` +
`mpsl_ptx_in_slot` Step 7 checks above.

Latest recorded hardware context from `session-ses_19c3.md`:

- `mpsl_3mode_poll_v7_dfu.zip` was flashed to `/dev/ttyACM0`.
- `mpsl_3mode_central_v6_dfu.zip` was flashed to `/dev/ttyACM1`.
- The debugging checklist at that point had completed PRX config review, PTX
  WaitAck review, pipe/address/frequency comparison, ACK timeout timing fix,
  and PRX pipe mask fix.
- Hardware verification was still in progress.
- PTX CDC output showed intermittent pipe 1 ACK loss, for example:

```text
r=10936 tx=1 ack=1 s=1 t0=1 rd=2 p1:1/1
r=10937 tx=1 ack=0 s=1 t0=164 rd=1 p1:0/1
r=10938 tx=1 ack=1 s=1 t0=1 rd=2 p1:1/1
```

Current no-hardware diagnostic configuration:

- Both `mpsl_3mode_poll` and `mpsl_3mode_central` now select
  `CoexistenceProfile::DiagnosticPipe1` rather than scattering raw slot
  constants through the examples.
- The diagnostic profile family now includes sweep variants for the next
  hardware tuning pass:
  - `DiagnosticPipe1`: PTX 1500 us slot, 400 us ACK timeout, 0 retries.
  - `DiagnosticPipe1RelaxedAck`: PTX 1500 us slot, 600 us ACK timeout,
    0 retries.
  - `DiagnosticPipe1Retry1`: PTX 1500 us slot, 400 us ACK timeout, 1 retry.
  - `DiagnosticPipe1LongSlot`: PTX 3000 us slot, 600 us ACK timeout, 1 retry.
  - `DiagnosticPipe1Prx8ms`: PRX 8000 us slot, PTX baseline.
  - `DiagnosticPipe1Prx12ms`: PRX 12000 us slot, PTX baseline.
  - `DiagnosticPipe1Prx20ms`: PRX 20000 us slot, PTX baseline.
- `mpsl_3mode_poll` profile values: 1500 us slot, 1300 us in-slot match, pipe
  1 only (`pipe_mask = 0x02`), one report per poll.
- `mpsl_3mode_central` profile values: 5000 us slot, 4500 us in-slot match,
  pipe 1 only (`enabled_pipes = 0x02`), 20 slots per report, no extra ESB idle
  delay.
- `PtxPollConfig` now makes ACK timeout and in-slot retry count explicit.
- The current diagnostic config uses `ack_timeout_us = 400` and
  `max_retries = 0`, so missed ACKs remain visible as diagnostic counters
  instead of being hidden by recovered retransmits.
- `PtxPollResult` reports incremental ACK timeout and ACK CRC-fail counts, both
  globally and per pipe.
- `PrxSlotResult` reports per-pipe RX, duplicate, bad-CRC, and ACK-TX counts.
- MPSL diagnostic protocol helpers for PID advance, pipe-mask round-robin,
  counter packet encoding/decoding, per-pipe deltas, and bounded spin loops are
  covered by host tests.
- PTX diagnostic RADIO disable waits now use a bounded helper instead of
  unbounded `EVENTS_DISABLED` spins. `SignalCounters::radio_disable_timeout`
  reports bounded waits that hit the spin limit.
- PTX log lines now include `to=<ack_timeout_count>` and
  `crc=<ack_crc_fail_count>`, plus `dt=<radio_disable_timeout>`:

```text
r=<round> tx=<n> ack=<n> to=<n> crc=<n> s=<start> t0=<timer0> rd=<radio> dt=<disable_timeout> p1:<ack>/<tx>/<to>/<crc>
b=<batch> rx=<n> dup=<n> crc=<n> p1:<rx>/<dup>/<crc>/<ack_tx> s=<start> t0=<timer0> rd=<radio> bk=<blocked> cn=<cancelled> dt=<disable_timeout>
```

For each `pN:` group, poll/TX uses `ack/tx/to/crc` and central/RX uses
`rx/dup/crc/ack_tx`.

No-hardware verification on 2026-06-01:

```bash
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
cargo check --example mpsl_smoke --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_basic --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_chained --features nrf52840,defmt,mpsl
cargo check --example mpsl_ptx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_ble --features nrf52840,defmt,mpsl
cargo check --example mpsl_ble_connectable --features nrf52840,defmt,mpsl
cargo check --example mpsl_3mode_poll --features nrf52840,defmt,mpsl
cargo check --example mpsl_3mode_central --features nrf52840,defmt,mpsl
cargo check --example mpsl_ptx_continuous --features nrf52840,defmt,mpsl
```

Result: all commands passed. The `nrf52840,mpsl,_cs-cortex` feature conflict
check still fails as expected with the explicit compile error. `git diff
--check` and targeted `rustfmt --check` for the touched MPSL files also pass.

Hardware pass on 2026-06-01:

- First re-established the exclusive ESB baseline with `prx_usb_dfu.zip` on
  `/dev/tty.usbmodemC2A1EFA145C41` and `ptx_ack_echo_dfu.zip` on
  `/dev/tty.usbmodemDC08665938A21`.
- PTX reached `tx=1200 ack_rx=1199` in a 12 second CDC capture, with
  monotonic ACK payload data and no observed lost/max-attempt burst.
- PRX CDC did not emit text during the short capture, but the PTX ACK payload
  stream confirmed that PRX was receiving and returning ACK payloads.
- Built current `mpsl_3mode_central_dfu.zip` and `mpsl_3mode_poll_dfu.zip`
  with `FEATURES=nrf52840,defmt,mpsl OBJCOPY=rust-objcopy`; the local host did
  not have `arm-none-eabi-objcopy` in `PATH`.
- Flashed `mpsl_3mode_central` to the first dongle and `mpsl_3mode_poll` to the
  second dongle while both were in Open DFU Bootloader.
- App CDC ports re-enumerated as `/dev/tty.usbmodem21201` for central/RX and
  `/dev/tty.usbmodem21301` for poll/TX.

Central/RX excerpt:

```text
b=620 rx=30 dup=0 crc=0 p0=0 p1=30 s=20 t0=20 rd=57 bk=0 cn=0 dt=0
[CUM] rx=10586 p0=0 p1=10586 dup=0 blk=0
```

Poll/TX excerpt:

```text
r=17000 tx=1 ack=1 to=0 crc=0 s=1 t0=1 rd=2 dt=0 p1:1/1
r=17004 tx=1 ack=0 to=1 crc=0 s=1 t0=2 rd=1 dt=0 p1:0/1
```

Result:

- The profile/radio-helper split does not prevent the 3-mode pair from
  starting, producing periodic reports, and exchanging pipe 1 packets.
- `bk/blk=0`, `cn=0`, and `dt=0` in the short sample; no blocked, cancelled, or
  radio-disable-timeout accumulation was observed.
- `to` is visible on poll misses and `crc` is only occasional. The short sample
  is suitable as a smoke result, not a stability result; follow it with a
  5-10 minute aggregate capture of `ack/to/crc/rd/blk/cn/dt`.

Five minute aggregate capture on 2026-06-01:

- Reused the same flashed `mpsl_3mode_central` and `mpsl_3mode_poll` pair.
- Captured central/RX from `/dev/tty.usbmodem21201` and poll/TX from
  `/dev/tty.usbmodem21301` for 300 seconds.
- Aggregation was done from CDC text lines instead of saving the full raw log.

Poll/TX trend:

```text
[poll +061s] lines=21016 rounds=102748-123763 tx=21016 ack=13669 to=7186 crc=161 ack_rate=0.6504 to_rate=0.3419 crc_rate=0.0077 rd_sum=34846 rd_avg=1.66 rd_max=2 dt_sum=0 dt_max=0
[poll +121s] lines=42054 rounds=102748-144801 tx=42054 ack=27317 to=14409 crc=328 ack_rate=0.6496 to_rate=0.3426 crc_rate=0.0078 rd_sum=69699 rd_avg=1.66 rd_max=2 dt_sum=0 dt_max=0
[poll +181s] lines=63062 rounds=102748-165809 tx=63062 ack=41045 to=21519 crc=498 ack_rate=0.6509 to_rate=0.3412 crc_rate=0.0079 rd_sum=104605 rd_avg=1.66 rd_max=2 dt_sum=0 dt_max=0
[poll +241s] lines=84084 rounds=102748-186831 tx=84084 ack=54734 to=28671 crc=679 ack_rate=0.6509 to_rate=0.3410 crc_rate=0.0081 rd_sum=139497 rd_avg=1.66 rd_max=2 dt_sum=0 dt_max=0
[poll final] lines=105096 rounds=102748-207843 tx=105096 ack=68394 to=35854 crc=848 ack_rate=0.6508 to_rate=0.3412 crc_rate=0.0081 rd_sum=174338 rd_avg=1.66 rd_max=2 dt_sum=0 dt_max=0
```

Central/RX trend:

```text
[central +061s] lines=480 blocks=2611-3090 rx=14426 p0=0 p1=14426 dup=0 crc=4 crc_per_rx=0.00028 rd_sum=28184 rd_avg=58.72 rd_max=68 bk=0 cn=0 dt_sum=0 dt_max=0
[central +121s] lines=959 blocks=2611-3569 rx=28764 p0=0 p1=28764 dup=0 crc=14 crc_per_rx=0.00049 rd_sum=56253 rd_avg=58.66 rd_max=69 bk=0 cn=0 dt_sum=0 dt_max=0
[central +181s] lines=1438 blocks=2611-4048 rx=43221 p0=0 p1=43221 dup=0 crc=22 crc_per_rx=0.00051 rd_sum=84512 rd_avg=58.77 rd_max=70 bk=0 cn=0 dt_sum=0 dt_max=0
[central +241s] lines=1916 blocks=2611-4526 rx=57588 p0=0 p1=57588 dup=0 crc=33 crc_per_rx=0.00057 rd_sum=112606 rd_avg=58.77 rd_max=72 bk=0 cn=0 dt_sum=0 dt_max=0
[central final] lines=2395 blocks=2611-5005 rx=71939 p0=0 p1=71939 dup=0 crc=42 crc_per_rx=0.00058 rd_sum=140688 rd_avg=58.74 rd_max=72 bk=0 cn=0 dt_sum=0 dt_max=0
```

Result:

- Poll timeout ratio was stable across the run: about 34.1-34.3% at each
  minute boundary, with final `to_rate=0.3412`.
- Poll ACK CRC-fail ratio was low and stable, ending at `crc_rate=0.0081`.
- Poll `rd` stayed bounded (`rd_avg=1.66`, `rd_max=2`) and
  `radio_disable_timeout` stayed at zero.
- Central/RX `rd` stayed bounded around the same average across the run
  (`rd_avg=58.66-58.77`, `rd_max=72`) rather than increasing monotonically.
- Central/RX `bk=0`, `cn=0`, and `dt=0` throughout the capture; no blocked,
  cancelled, or radio-disable-timeout accumulation was observed.
- The remaining misses are stable ACK-window misses in the current diagnostic
  profile, not an obvious profile/radio-helper regression or runaway MPSL
  scheduling failure.

Next profile sweep procedure:

1. Edit `PROFILE` in both `examples/mpsl_3mode_poll.rs` and
   `examples/mpsl_3mode_central.rs`.
2. Use the same central profile for all narrow pipe-1 sweep runs unless the PRX
   side is explicitly being tuned; the current sweep variants mostly change PTX
   ACK wait/retry/slot behavior.
3. Build and flash `mpsl_3mode_central` and `mpsl_3mode_poll`.
4. Capture each profile for 2-5 minutes.
5. Compare:
   - poll `ack_rate`, `to_rate`, `crc_rate`, `rd_max`, and `dt_max`;
   - poll per-pipe `p1:<ack>/<tx>/<to>/<crc>`;
   - central per-pipe `p1:<rx>/<dup>/<crc>/<ack_tx>`;
   - central `bk/cn/dt` and whether `rd` remains bounded.

Suggested order:

1. `DiagnosticPipe1` baseline.
2. `DiagnosticPipe1RelaxedAck` to test whether 400 us ACK wait is too short.
3. `DiagnosticPipe1Retry1` to test whether one in-slot retry recovers misses.
4. `DiagnosticPipe1LongSlot` to test whether extra slot budget changes both
   `to_rate` and `rd/dt`.

### 2026-06-02 Hardware Profile Sweep

Setup:

- Pulled `feat/mpsl-timeslot` to `4db1e1e`.
- Rebuilt each profile with
  `make -B mpsl_3mode_central_dfu.zip mpsl_3mode_poll_dfu.zip FEATURES=nrf52840,defmt,mpsl OBJCOPY=rust-objcopy`.
- Flashed `mpsl_3mode_central` to `/dev/tty.usbmodemC2A1EFA145C41`
  and `mpsl_3mode_poll` to `/dev/tty.usbmodemDC08665938A21`.
- Captured each profile for 180 seconds from app CDC ports
  `/dev/tty.usbmodem21201` and `/dev/tty.usbmodem21301`.

Summary:

| Profile | Poll ack_rate | Poll to_rate | Poll crc_rate | Poll rd_max | Poll dt_max | Poll p1 ack/tx/to/crc | Central rx | Central dup | Central crc | Central ack_tx | Central rd_max | Central bk/cn/dt | Central p1 rx/dup/crc/ack_tx |
| --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| `DiagnosticPipe1` | 0.6533 | 0.3418 | 0.0049 | 2 | 0 | 41232/63110/21570/308 | 43225 | 0 | 26 | 43225 | 70 | 0/0/0 | 43225/0/26/43225 |
| `DiagnosticPipe1RelaxedAck` | 0.6531 | 0.3418 | 0.0051 | 2 | 0 | 41183/63061/21556/322 | 43171 | 0 | 15 | 43171 | 69 | 0/0/0 | 43171/0/15/43171 |
| `DiagnosticPipe1Retry1` | 0.6507 | 0.3446 | 0.0047 | 2 | 0 | 41050/63084/21738/296 | 43014 | 0 | 20 | 43014 | 68 | 0/0/0 | 43014/0/20/43014 |
| `DiagnosticPipe1LongSlot` | 0.6522 | 0.3435 | 0.0044 | 2 | 0 | 26634/40840/14028/178 | 27944 | 0 | 9 | 27944 | 41 | 0/0/0 | 27944/0/9/27944 |

Observations:

- The new diagnostic log format worked in all four profiles, including poll
  `p1:<ack>/<tx>/<to>/<crc>` and central
  `p1:<rx>/<dup>/<crc>/<ack_tx>`.
- `DiagnosticPipe1RelaxedAck` did not improve timeout rate over the baseline:
  both ended at `to_rate=0.3418`, so the 400 us ACK wait does not appear to be
  the limiting factor by itself.
- `DiagnosticPipe1Retry1` did not convert timeouts into ACKs; it ended slightly
  worse than baseline at `ack_rate=0.6507` and `to_rate=0.3446`.
- `DiagnosticPipe1LongSlot` also did not reduce timeout rate
  (`to_rate=0.3435`). It reduced central `rd_max` from about 68-70 to 41, but
  poll throughput dropped as expected because the PTX slot length doubled.
- All four profiles kept poll `dt_max=0` and central `bk=0`, `cn=0`, `dt=0`.
  There was no evidence of blocked/cancelled/disable-timeout accumulation.
- Comparing poll `p1 tx` to central `p1 rx` shows many more poll attempts than
  central receives in every profile. Comparing central `p1 ack_tx` to poll
  `p1 ack` shows central ACK transmissions track received packets, while poll
  still misses a similar fraction of ACKs. Poll `p1 to` dominates `p1 crc`,
  so the remaining loss mode is mostly timeout, not CRC fail.

Follow-up implementation after profile sweep:

- The four profile variants produced essentially the same timeout rate in the
  external hardware run summary (`to_rate` stayed around 34% while
  `bk/cn/dt=0`). That makes ACK wait length, one retry, and longer PTX slot
  unlikely to be the primary cause.
- PRX long-lived reporting now keeps chaining timeslots from the TIMER0
  callback when `report_every` is reached. It marks `report_ready`, wakes the
  async task, and returns `MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST` instead of
  ending the session and waiting for task-side re-request.
- This change is intended to remove the PRX receive gap caused by returning a
  report through `SESSION_IDLE` before requesting the next PRX slot.

Next hardware check:

1. Rebuild and flash `mpsl_3mode_central` and `mpsl_3mode_poll` with the
   default `DiagnosticPipe1` profile.
2. Run a 2-5 minute capture.
3. Compare the new baseline against the earlier `DiagnosticPipe1`
   `ack_rate=0.6533`, `to_rate=0.3418`, `crc_rate=0.0049`, `bk/cn/dt=0`.
4. Use the per-pipe fields to classify the result:
   - central `p1 rx` far below poll `p1 tx`: PRX receive window is still the
     likely limiter.
   - central `p1 ack_tx` far above poll `p1 ack`: ACK return path/timing is the
     likely limiter.
   - central `p1 rx` and `ack_tx` both track poll `p1 ack`: PTX packets are
     mostly arriving only during PRX windows.

### 2026-06-02 Continuous PRX Report Retest

Setup:

- Pulled `feat/mpsl-timeslot` to `6aff0cc`.
- Rebuilt the default `DiagnosticPipe1` pair with
  `make -B mpsl_3mode_central_dfu.zip mpsl_3mode_poll_dfu.zip FEATURES=nrf52840,defmt,mpsl OBJCOPY=rust-objcopy`.
- Flashed `mpsl_3mode_central` to `/dev/tty.usbmodemC2A1EFA145C41`
  and `mpsl_3mode_poll` to `/dev/tty.usbmodemDC08665938A21`.
- Captured 180 seconds from app CDC ports `/dev/tty.usbmodem21201` and
  `/dev/tty.usbmodem21301`.

Result:

| Run | Poll ack_rate | Poll to_rate | Poll crc_rate | Poll rd_max | Poll dt_max | Poll p1 ack/tx/to/crc | Central rx | Central dup | Central crc | Central ack_tx | Central rd_max | Central bk/cn/dt | Central p1 rx/dup/crc/ack_tx |
| --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| Earlier `DiagnosticPipe1` baseline | 0.6533 | 0.3418 | 0.0049 | 2 | 0 | 41232/63110/21570/308 | 43225 | 0 | 26 | 43225 | 70 | 0/0/0 | 43225/0/26/43225 |
| Continuous PRX report retest | 0.6573 | 0.3379 | 0.0048 | 2 | 0 | 41491/63126/21331/304 | 43426 | 0 | 19 | 43426 | 68 | 0/0/0 | 43426/0/19/43426 |

Observation:

- The continuous PRX report change produced only a small timeout improvement:
  `to_rate` moved from `0.3418` to `0.3379` and `ack_rate` moved from
  `0.6533` to `0.6573`.
- This is not a large enough drop to identify the former PRX report gap as the
  dominant source of the roughly 34% timeout rate.
- Poll `p1 tx=63126` remains well above central `p1 rx=43426`, while central
  `p1 ack_tx=43426` is only modestly above poll `p1 ack=41491`. That points
  more strongly at PTX packets not landing in the PRX receive window than at a
  pure ACK return-path failure.
- Poll `p1 to=21331` still dominates `p1 crc=304`, so the remaining loss mode
  is still primarily timeout rather than CRC fail.
- Poll `dt_max=0` and central `bk=0`, `cn=0`, `dt=0`; no MPSL
  blocked/cancelled/disable-timeout regression was observed.
- Derived rates:
  - `prx_rx_rate = central.p1_rx / poll.p1_tx = 43426 / 63126 = 0.688`.
  - `ack_return_rate = poll.p1_ack / central.p1_ack_tx = 41491 / 43426 = 0.955`.
- This points to PRX receive-window coverage as the main loss source. The ACK
  return path still has some loss, but it is not the first-order limiter.

Next PRX duty sweep:

1. Keep the poll/PTX side on the baseline PTX values by selecting the same
   profile name in both 3-mode examples.
2. Test these profile values in order:
   - `DiagnosticPipe1Prx8ms`
   - `DiagnosticPipe1Prx12ms`
   - `DiagnosticPipe1Prx20ms`
3. Capture each profile for 2-5 minutes.
4. Compare:
   - `prx_rx_rate = central.p1_rx / poll.p1_tx`
   - `ack_return_rate = poll.p1_ack / central.p1_ack_tx`
   - poll `to_rate` and `crc_rate`
   - central `bk/cn/dt` and `rd_max`
 5. A useful improvement should raise `prx_rx_rate` and reduce `to_rate` without
   introducing blocked/cancelled/disable-timeout accumulation.

### 2026-06-03 PRX Duty Sweep

Setup:

- Pulled `feat/mpsl-timeslot` to `ca44389`.
- Rebuilt each profile with
  `make -B mpsl_3mode_central_dfu.zip mpsl_3mode_poll_dfu.zip FEATURES=nrf52840,defmt,mpsl OBJCOPY=rust-objcopy`.
- Flashed `mpsl_3mode_central` to `/dev/tty.usbmodemC2A1EFA145C41`
  and `mpsl_3mode_poll` to `/dev/tty.usbmodemDC08665938A21`.
- Captured each profile for 180 seconds from app CDC ports
  `/dev/tty.usbmodem21201` and `/dev/tty.usbmodem21301`.

Summary:

| Profile | Poll ack_rate | Poll to_rate | Poll crc_rate | Poll rd_max | Poll dt_max | Poll p1 ack/tx/to/crc | Central rx | Central dup | Central crc | Central ack_tx | Central rd_max | Central bk/cn/dt | Central p1 rx/dup/crc/ack_tx |
| --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| `DiagnosticPipe1Prx8ms` | 0.7506 | 0.2468 | 0.0026 | 2 | 0 | 47286/62999/15547/166 | 48517 | 0 | 3 | 48517 | 69 | 0/0/0 | 48517/0/3/48517 |
| `DiagnosticPipe1Prx12ms` | 0.7949 | 0.2029 | 0.0022 | 2 | 0 | 50120/63049/12792/137 | 51000 | 0 | 0 | 51000 | 67 | 0/0/0 | 51000/0/0/51000 |
| `DiagnosticPipe1Prx20ms` | 0.8179 | 0.1806 | 0.0015 | 2 | 0 | 51548/63023/11382/93 | 52087 | 0 | 8 | 52087 | 71 | 0/0/0 | 52087/0/8/52087 |

Derived rates:

- `prx_rx_rate`: 0.770 (8ms), 0.809 (12ms), 0.827 (20ms).
- `ack_return_rate`: 0.975 (8ms), 0.983 (12ms), 0.990 (20ms).

Observations:

- All three PRX duty sweep profiles kept `bk/cn/dt=0` and `rd_max` bounded.
- PRX slot length is the primary lever for receive-window coverage. 12ms is a
  good balance point; 12ms→20ms has diminishing returns.
- The remaining ~20% loss is dominated by PTX packets landing outside PRX
  receive windows (timeout), not by CRC failure.

### 2026-06-03 Event-Driven PTX

Added `PtxEventSession` API and `mpsl_3mode_event` example for event-driven
ESB transmissions with cross-window retry and pending report retention.

Hardware test (120s, Prx12ms profile, 5 cross-window retries, 2ms retry delay):

- PTX events: 2600
- PTX acked: 2600 (**100% event-level reliability**)
- PTX total sends: 6569 (avg 2.52 per event)
- Central rx: 2606, dup=0, blk=0
- Pending retries: 34 (all during central cold-start)

Estimated performance for keyboard use case:

- Typical latency: ~3ms (first-send ACK)
- Worst-case latency: ~73ms (5 retries fail + pending to next 50ms cycle)
- Idle current: <10μA (deep sleep, GPIO wake)
- Typing current: ~0.8mA at 10 events/sec
- CR2032 estimated life: ~6 months mixed use

### 2026-06-03 11ms PRX Slot Diagnosis

Context:

- The PRX schedule implementation was moved to a chained timeslot model and the
  event/PTX path now reports per-event attempts.
- `DiagnosticPipe1Prx12ms` was temporarily edited for diagnosis to use an
  11ms PRX slot with a 10.5ms in-slot match window.
- Direction B for this pass was to determine whether the earlier bad 11ms run
  was a real slot-length regression or a startup/session anomaly.

Reference data:

| Run | Duration | ack_rate | avg_att | fail/pending | crc | rd_avg/max | bk/batch | attempt_dist |
| --- | ---: | ---: | ---: | --- | ---: | --- | --- | --- |
| 12ms baseline | 300s | 1.0000 | 1.11 | 0/0 | 0 | 3.95/6 | 2860/2868 = 0.997 | `{1: 5087, 2: 442, 3: 61, 4: 21}` |
| 10ms short run | 120s | 1.0000 | 1.16 | 0/0 | 9 | 3.24/7 | 1143/1403 = 0.815 | `{1: 1912, 2: 286, 3: 25, 4: 10}` |
| 11ms abnormal first run | 120s | 1.0000 for completed events | 1.40 completed, 5.0 pending | 0/1617 | 0 | 3.61/7 | 30/33 = 0.91 | `{1: 42, 2: 10, 3: 1, 4: 1, 5: 1617}` |
| 11ms reflash retest | 120s | 1.0000 | 1.12 | 0/0 | 0 | 3.56/5 | 1148/1278 = 0.898 | `{1: 2015, 2: 194, 3: 21, 4: 12}` |
| 11ms diagnosis retest | 120s | 1.0000 | 1.12 | 0/0 | 0 | 3.56/5 | 1147/1278 = 0.898 | `{1: 2020, 2: 194, 3: 25, 4: 5}` |
| 11ms diagnosis stability | 300s | 1.0000 | 1.12 | 0/0 | 0 | 3.55/5 | 2860/3190 = 0.897 | `{1: 5051, 2: 491, 3: 41, 4: 27}` |
| 11ms startup-diagnostics retest | 120s | 1.0000 | 1.12 | 0/0 | 0 | 3.56/5 | 1145/1275 = 0.898 | `{1: 2015, 2: 192, 3: 19, 4: 13}` |

Additional diagnostics from central reports:

- `cfg=11000/10500` confirmed the intended 11ms diagnostic profile was running.
- `cn=0`, `dt=0`, `si=0`, `sc=0`, `ov=0`, and `iv=0` stayed at zero in the
  healthy 11ms runs.
- `bk` is dominated by NORMAL chained timeslot requests: short central reads
  showed `nb` increasing with `bk`, while `eb`, `nc`, and `ec` stayed at zero.
  This points to normal-chain request conflicts, not EARLIEST recovery failure.
- The startup-diagnostics build adds `[START]` lines on both devices. Central
  reports PRX profile config, pipe mask, request timeout, high-priority retry
  policy, and PRX schedule config. Event reports PTX slot config, pipe, ACK
  timeout, retry count, request timeout, and high-priority retry policy.
- Event report lines now append per-event aggregated counters:
  `s/t0/rd/bk/cn/dt/si/sc/ov/iv`. The legacy `e=... ok=... att=... pend=...`
  prefix is unchanged, so the existing statistics script still parses it.
- After DFU into the startup-diagnostics build, Event initially repeated
  `e=1 ok=false att=5 pend=true` while Central was still starting. The Event
  counters for those retries were `s=5 t0=10 rd=5 bk=0 cn=0 dt=0 si=0 sc=0
  ov=0 iv=0`. Once Central PRX reports were active, Event recovered to stable
  `ok=true` without session lifecycle counters incrementing.

Conclusion:

- 11ms is a valid candidate in the healthy state. It reduces blocked pressure
  versus 12ms by roughly 10% (`bk/batch` about 0.90 vs 1.00) while keeping
  `ack_rate=1.0`, `avg_att` around 1.12, and `rd_max` bounded at 5.
- The earlier bad 11ms run did not reproduce after reflash/retest and 5 minutes
  of continuous sampling. Treat it as an occasional startup/session anomaly
  until a failing run is captured with the new diagnostics.
- One 300s script launch using `DURATION_S=300 python3 ...` failed because
  `/dev/ttyACM0` was not present at open time, but the port immediately
  reappeared. A direct `python3 /tmp/esb_event_stats.py` run succeeded. This
  reinforces that future anomaly capture should log USB/boot/session state
  separately from radio counters.
- The observed startup pending pattern is currently consistent with PRX/PTX
  cold-start alignment rather than MPSL session corruption: Event had no
  blocked/cancelled/disable-timeout/session-idle/session-closed/invalid/overstay
  counters while pending, and recovered without intervention.

Next direction B steps:

1. Keep 11ms as a diagnostic candidate, but do not make it the final default
   until multiple cold-start or DFU-start cycles are captured.
2. Repeat several cold-start or DFU-start cycles with the startup-diagnostics
   build and check whether Event pending always clears after Central PRX starts.
3. During a failing run, distinguish these cases:
   - NORMAL chain blocked loop: `nb` grows, `eb/ec` stay zero, Event pending
     accumulates.
   - EARLIEST recovery issue: `eb` or `ec` grows.
   - session lifecycle issue: `si/sc/iv/ov` grows.
   - USB/reboot issue: CDC port disappears or event counters restart.

## Pending Work

- Add GATT echo/notify once the basic advertising + ESB PRX coexistence smoke test passes.
- Run Step 8 keyboard-style split scenario with 7.5 ms BLE connection interval.
- Move review fixes from `docs/review-fix-backlog.md` only after M10 first-pass validation is complete.
- Integrate real key matrix scanning into `mpsl_3mode_event` to replace the 50ms mock timer.
