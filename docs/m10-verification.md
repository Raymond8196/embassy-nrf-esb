# M10: MPSL Timeslot Adapter Verification

Created: 2026-05-19

This document tracks M10 verification separately from the implementation plan in `docs/m10-plan.md`. Hardware validation is pending for the latest Step 7 work because no dongles are currently available.

## Current Status

| Step | Scope | Status | Notes |
|------|-------|--------|-------|
| 0 | MPSL init smoke | Build-only current pass | Hardware result not re-run in this pass |
| 1 | Single timeslot request | Build-only current pass | Hardware result not re-run in this pass |
| 2 | Chained timeslots | Build-only current pass | Hardware result not re-run in this pass |
| 3-4 | PTX-in-timeslot + PID continuity | Build-only current pass | Existing `mpsl_ptx_in_slot` compiles |
| 5-6 | PRX-in-timeslot + multi-pipe ACK payload | Build-only current pass | Existing `mpsl_prx_in_slot` compiles |
| 7 | BLE coexistence first pass | Compile-only ready | New `mpsl_prx_ble` advertises `ESB M10` and enters PRX timeslots |
| 8-9 | Split E2E + overnight | Not started | Requires two dongles and BLE central/PC validation |

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

## Pending Work

- Tune pipe 1 ACK coverage under active BLE connection; current relaxed CI run is functional but still below advertising-only throughput.
- Add GATT echo/notify once the basic advertising + ESB PRX coexistence smoke test passes.
- Run Step 8 keyboard-style split scenario with 7.5 ms BLE connection interval.
- Move review fixes from `docs/review-fix-backlog.md` only after M10 first-pass validation is complete.
