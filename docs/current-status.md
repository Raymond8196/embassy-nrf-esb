# Current Status

Updated: 2026-07-03

This is the active status entry point. Older plans are kept in `docs/archive/`
when they are useful as history but no longer describe the next work.

## Active Focus

The current product focus is Elytra G1: right half PTX to left half PRX, with
the left half forwarding HID over BLE. The latest implementation uses
radio-notification-driven parked PRX windows plus adaptive fast windows while
typing. See `radio-notification-self-trigger.md` for the measurement details.

The current engineering focus is validating that parked/adaptive PRX is stable
enough for daily use before moving deeper into G2 dongle mode or the RMK-side
adapter.

## Branch Notes

After branch consolidation, use `main` as the active development branch. The
status cleanup was prepared on `codex/radio-notification-rx` and then promoted
to `main`.

## What Is Verified

- Core exclusive ESB is alpha-quality and hardware verified for PTX/PRX, ACK
  payloads, multi-pipe, NoAck, suspend/resume, and long idle recovery.
- MPSL BLE + ESB coexistence has been hardware verified in diagnostic examples.
- Elytra G1 parked PRX receives right-half packets in BLE gaps without the
  originally feared self-trigger loop.
- Adaptive fast cadence fixes the main right-key latency problem: steady-state
  right-key latency is around 13 ms median, with retry tails around 16-20 ms and
  first-key wake from parked idle around 30 ms.
- Held-key keepalive and link-loss handling are in place so long key holds do
  not get released by the watchdog while the key is still physically held.

## Known Limits

- BLE idle power is host-capped on macOS: the host keeps HID slave latency at 0,
  so the left half must wake for every BLE connection event. Exact post-fix idle
  numbers still need PPK2 measurement.
- The co-channel dongle case is still open. A powered dongle using the same ESB
  address can interfere with the left half; the next fix is distinct G1/G2 ESB
  addresses followed by hardware verification with the dongle powered.
- The single-engine MPSL convergence work is implemented behind guarded paths,
  but full PRX/PTX S5/S7 hardware replay is still pending.
- The RMK `SplitReader` / `SplitWriter` adapter is not in this repository. It
  belongs in the RMK repo because RMK owns `SplitMessage` and the split traits.
- G2 dongle mode and runtime G1/G2 switching remain product work, not current
  baseline behavior.

## Next Work

1. Validate the two local Elytra changes on hardware: BLE stability, right-key
   latency/retry rate, and RTT behavior without a debugger attached.
2. Give the dongle and G1 halves distinct ESB address sets; verify the left half
   stays stable with the dongle powered.
3. Record PPK2 idle current after the HFXO-leak fixes and the 6 ms cadence tune.
4. Update `radio-notification-self-trigger.md` with the validation results.
5. Choose the next product track: RMK adapter first, or G2 dongle bring-up first.

## Active Documents

- `radio-notification-self-trigger.md` - current Elytra parked/adaptive PRX
  findings and open issue.
- `elytra-production-plan.md` - product targets for G1, G2, and manual runtime
  switching.
- `single-engine-convergence-plan.md` - MPSL single-engine convergence progress.
- `transport-ack.md` - application-level framing and transport ACK protocol.
- `rmk-integration.md` - intended RMK adapter boundary and frame usage.
- `core-verification.md` - exclusive ESB verification record.
- `alpha-testing.md` - external alpha build and smoke-test guide.
