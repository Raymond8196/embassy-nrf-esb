# MPSL / RMK Architecture References

Created: 2026-06-01

This document records mature projects and references worth studying while
evolving this crate from ESB diagnostics toward an RMK-ready split transport.

## What To Copy Into This Repository

Do not copy implementation code directly across languages or frameworks. Copy
the architecture boundaries, scheduling assumptions, API shape, and validation
methods.

| Source | What to learn | Local application |
|--------|---------------|-------------------|
| `too1/ncs-esb-ble-mpsl-demo` | BLE and ESB coexistence through MPSL timeslots; separation between application ESB facade and timeslot handler | Keep MPSL session, RADIO handoff, PRX/PTX protocol logic, and diagnostics as separate modules |
| Nordic MPSL timeslot guidance | MPSL request timing, blocked/cancelled recovery, BLE connection-event interaction, non-reentrant MPSL constraints | Treat profiles as scheduling policies, not just slot constants |
| RMK | Rust/Embassy keyboard task structure, split transport contracts, postcard payload flow, BLE/USB output ownership | Keep RMK adapter outside this crate; expose stable ESB framing and diagnostics here |
| RMK real-world examples | Board-level config, BLE/USB dual-mode examples, payload sizing assumptions | Validate ESB payload length from RMK split message size before runtime |
| ZMK | Product topology for split keyboards and dongle mode | Use clear central/peripheral/dongle role boundaries in docs and APIs |
| Embassy / embassy-nrf | Owned driver style, async API, interrupt binding, narrow unsafe boundary | Move MPSL diagnostics toward owned handles instead of free functions backed by global state |
| `nrf-mpsl` Rust crate examples | Rust MPSL session memory, callback pattern, interrupt integration | Keep callback state minimal and document raw mutex assumptions |

## Borrowed Items Already Added

The following ideas have been folded into this repository:

1. **Single profile source**

   `CoexistenceProfileConfig` is now the single source for profile-level PRX,
   PTX, timeslot request, RADIO recovery, and BLE coexistence assumptions.
   `PrxSlotConfig::for_profile()` and `PtxPollConfig::for_profile()` continue
   to work, but both derive from the same profile config.

2. **Explicit RADIO handoff semantics**

   RADIO shutdown before MPSL handoff now uses typed results:

   - `RadioDisableResult`
   - `RadioQuiesceResult`
   - `RadioRecoveryPolicy`

   This keeps timeout handling visible instead of hiding it behind a boolean.

3. **MPSL module split**

   The first split has moved profile and RADIO handoff code out of
   `mpsl_timeslot.rs`:

   - `src/mpsl_profile.rs`
   - `src/mpsl_radio.rs`

   `mpsl_timeslot.rs` still owns callback-heavy PRX/PTX logic for now.

4. **Diagnostic profile sweep and per-pipe counters**

   The `DiagnosticPipe1` family now includes relaxed ACK, retry, and long-slot
   variants. The 3-mode logs expose per-pipe poll `ack/tx/to/crc` and central
   `rx/dup/crc/ack_tx` so the next hardware run can distinguish missed ACK
   windows from CRC failures and central-side receive/ACK behavior.

## Recommended Study Order

1. Read `too1/ncs-esb-ble-mpsl-demo` first for the closest BLE/ESB/MPSL
   coexistence shape.
2. Read Nordic's MPSL timeslot material next to understand scheduling and BLE
   connection-event constraints.
3. Read RMK split and wireless docs to understand the Rust keyboard integration
   target.
4. Read ZMK split/dongle docs to calibrate product topology and role naming.
5. Read Embassy and `nrf-mpsl` examples to keep the Rust driver boundary clean.

## Concrete Local Follow-Ups

These are good no-dongle development tasks:

1. Move MPSL diagnostics into more files:
   - `mpsl_diagnostics.rs`
   - `mpsl_prx.rs`
   - `mpsl_ptx.rs`
   - `mpsl_session.rs`
2. Extract PRX duplicate/no-ack/ACK decision logic into pure helper functions.
3. Add host tests for profile validation and PRX protocol decisions.
4. Define an owned `MpslEsbCoordinator` facade after hardware behavior is
   stable enough to choose the right lifecycle contract.
5. Keep RMK-specific `SplitReader` / `SplitWriter` implementation in RMK, not
   in this crate.

## Reference Links

- ncs-esb-ble-mpsl-demo: <https://github.com/too1/ncs-esb-ble-mpsl-demo>
- Nordic MPSL timeslot article: <https://devzone.nordicsemi.com/guides/nrf-connect-sdk-guides/b/software/posts/updating-to-the-mpsl-timeslot-interface>
- RMK: <https://github.com/HaoboGu/rmk>
- RMK wireless docs: <https://rmk.rs/main/docs/features/wireless>
- RMK real-world examples: <https://main.rmk.rs/main/docs/getting_started/real_world_examples>
- ZMK split keyboard docs: <https://zmk.dev/docs/features/split-keyboards>
- Embassy: <https://github.com/embassy-rs/embassy>
- nrf-mpsl crate docs: <https://docs.rs/nrf-mpsl/latest/nrf_mpsl/>
