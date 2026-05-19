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

## Pending Work

- Extend Step 7 from advertising-only coexistence to BLE connection stability.
- Add GATT echo/notify once the basic advertising + ESB PRX coexistence smoke test passes.
- Run Step 8 keyboard-style split scenario with 7.5 ms BLE connection interval.
- Move review fixes from `docs/review-fix-backlog.md` only after M10 first-pass validation is complete.
