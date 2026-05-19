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

## Pending Work

- Extend Step 7 from advertising-only coexistence to BLE connection stability.
- Add GATT echo/notify once the basic advertising + ESB PRX coexistence smoke test passes.
- Run Step 8 keyboard-style split scenario with 7.5 ms BLE connection interval.
- Move review fixes from `docs/review-fix-backlog.md` only after M10 first-pass validation is complete.
