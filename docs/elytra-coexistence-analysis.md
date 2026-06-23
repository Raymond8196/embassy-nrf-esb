# Elytra BLE+ESB Coexistence: Latency & Power Analysis

Created: 2026-06-24

Theoretical analysis of the elytra split keyboard's BLE+ESB coexistence
architecture vs a pure-BLE split (RMK), to ground future tuning. Numbers are
derived from radio current (~5.5mA RX @2Mbps) + duty cycle, not measured —
confirm with a PPK2 before productizing.

## Architecture

- Right half (nRF52833, `elytra-event`): matrix scan → ESB event PTX → left
  half. No BLE here; event-driven, idle issues zero timeslot requests.
- Left half (nRF52833, `elytra-left-central`): matrix scan + ESB PRX from right
  + aggregate + BLE HID to host.
- Both halves use `CoexistenceProfile::NordicExtend` (1500us initial slot +
  530us EXTEND; PRX schedule = `continuous`).

A right-half keypress is two radio hops (ESB right→left, then BLE left→host) —
**same hop count as an RMK split** (BLE-split right→left, then BLE left→host).
The difference is the first hop's transport (ESB vs BLE).

## Latency (theoretical)

| Path | elytra (host BLE 15ms) | RMK split (host 7.5ms, split BLE 7.5ms) |
|---|---|---|
| Left (central) keypress | scan 2 + host avg 7.5 ≈ **9.5ms** | scan 2 + host avg 3.75 ≈ **5.75ms** |
| Right (peripheral) keypress | scan 2 + ESB ~1ms + host avg 7.5 ≈ **10.5ms** | scan 2 + split BLE avg 3.75 + host avg 3.75 ≈ **9.5ms** |

Right-half latency is **comparable** (~10.5 vs ~9.5ms), not worse — the ESB hop
(~1ms) is actually faster than RMK's BLE-split hop (avg 3.75ms) because the PRX
listens continuously. The gap is the 15ms vs 7.5ms host BLE interval; switching
host BLE to 7.5ms makes elytra's right half (~6.75ms) beat RMK split.

## Power (theoretical — the real problem)

`NordicExtend` PRX = `continuous` schedule + EXTEND ⇒ the left half listens
almost continuously:

- 1500us initial + 530us × 200 extends ≈ 108ms RX chain, immediately re-chained
  ⇒ ~80% of time in RX (after subtracting BLE conn events).
- nRF52833 RX @2Mbps ≈ 5.5mA ⇒ **left half idle ≈ 4–5mA**.

| Half | elytra idle | RMK split idle |
|---|---|---|
| Left (central) | **~4–5mA** (continuous RX) | ~tens of uA (periodic BLE conn events) |
| Right (peripheral) | ~few uA (event ESB, no BLE) | ~tens of uA |

The left half dominates total power (~100x RMK's left). Battery life estimate
(1000mAh, idle): elytra ~9 days vs RMK ~800+ days. The ~1ms ESB-hop latency
advantage is bought with this continuous-RX cost — the tradeoff to attack for
any battery-powered product.

## Improvement direction: `NordicExtendPaced` profile

Make the PRX hint-driven instead of continuous: open a short timeslot only when
the right half's schedule hint says it will transmit.

| PRX period | left idle current | right latency (avg period/2 + 1ms) |
|---|---|---|
| continuous (current) | ~4–5mA | ~1ms |
| 10ms | ~550uA | ~6ms |
| **30ms (suggested)** | **~170uA** | **~16ms** |
| 100ms | ~55uA | ~51ms (too slow) |

Suggested target: period 20–30ms ⇒ left idle ~150–200uA (~25–30x lower), right
latency ~10–15ms (still near RMK split). Long-term best option is adaptive
(large period when idle, continuous/small-period while typing).

Costs / risks:
- OK rate drops: hint misalignment can miss packets ⇒ must add PTX retries
  (current `max_retries=0`). Expect 100% → ~95–99%.
- Re-verify OK rate, right latency, and BLE coexistence for each period.

## Current status (2026-06-24)

- Host BLE conn params now actively requested: `request_conn_params()` in
  `boards/elytra/src/bin/left_central.rs` sends L2CAP 0x12 after encryption
  with 15ms / latency 30 / timeout 5s (keyboard-firmware values per RMK/ZMK;
  not the prototype's 100ms coexistence value). Fixes the intermittent idle BLE
  disconnect (the port had dropped the active param update). Long-run
  confirmation pending.
- Open work: verify 7.5ms host BLE coexistence; design/implement
  `NordicExtendPaced` for power; PPK2 measurement to confirm the ~4-5mA left
  idle figure.
