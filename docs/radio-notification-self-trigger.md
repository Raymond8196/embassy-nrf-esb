# Radio Notification × Parked ESB RX: Self-Trigger Findings

Created: 2026-06-25 — branch `codex/radio-notification-rx`

Goal: use `mpsl_radio_notification_cfg_set` (`INT_ON_INACTIVE`) to open a short
ESB RX window in each BLE gap, instead of continuous PRX — to cut the left
half's idle current (NordicExtend continuous PRX ≈ 4–5mA; see
`elytra-coexistence-analysis.md`).

## The self-trigger risk (raised in review)

`cb(INACTIVE) → request_window → open ESB RX slot → slot ends → if that end
also fires INACTIVE → cb again → request_window → …` = steady continuous PRX,
power goal gone.

## Step 1 — observe (continuous PRX baseline)

Registered `mpsl_radio_notification_cfg_set(INT_ON_BOTH)` after MPSL init, before
SDC start. cb counts ACTIVE/INACTIVE; `radio_obs_task` prints /s.

Result (continuous PRX running, confirmed via live split traffic):
```
active = inactive = 66/s   (= BLE conn-event rate, 15ms interval)
```
ESB continuous PRX radio is active, yet notification == BLE-only rate.

## Step 2 — parked RX window probe

`prx_parked_test_task`: `open_parked_prx_session`; wait on BLE-INACTIVE signal;
call `request_window`; count req/s and coalesced/s.

Result (28 periods, ~56s, stable):
```
active = inactive = req = 66/s   coal = 0   (no panic / error / OVERSTAYED)
```
req/s == BLE conn rate. **No self-trigger loop.**

## Why the naive doc reading was wrong

Nordic docs: *"with sufficient time between timeslot events, both ACTIVE and
nACTIVE are present at each event; without sufficient time, skipped."* Naive
read: parked-window spacing = BLE interval (15ms, "sufficient") ⇒ each window
notifies ⇒ self-loop.

Actual mechanism: `cb(INACTIVE) → request_window` submits an **EARLIEST**
request ⇒ MPSL grants the window **immediately after the BLE event** (radio
continuous: BLE end → ESB window start, tiny gap) ⇒ window is back-to-back with
the BLE event ⇒ "insufficient time" ⇒ the window's ACTIVE/INACTIVE are
**merged/skipped** ⇒ only the BLE edge fires cb. Continuous PRX is the same
story (slot chaining, back-to-back slots ⇒ skipped).

So ESB timeslots — whether continuous-chained or parked-earliest — are always
back-to-back with adjacent radio and get merged out of the notification stream.
Only BLE (protocol stack) edges fire the cb.

## Conclusion

- **Parked RX windows do NOT self-trigger.** No self-mask needed.
- The radio-notification-driven parked-RX design is viable as-is for the
  self-trigger concern.

## Step 3 — receive verification (done)

Parked windows **do receive** right-half packets. Verified on hardware:

- Right snapshot seq 195..219 chg=1, contiguous — keypresses received in
  BLE-gap windows with no drops/dupes.
- req=66/s coal=0 steady — still no self-trigger with receive enabled.
- End-to-end: right-half keys produce characters on host (BLE HID), no
  perceivable latency/stuck-key issues vs continuous PRX.

Right half needs **no change** — it stays event-driven (PTX sends on key press,
retries until ACK). The parked window in each BLE gap is enough to catch the
retransmitted packet.

## Production status

The parked PRX path is now the default in `left_central.rs` (continuous PRX
removed). The self-trigger probe instrumentation has been cleaned up; only
`BLE_INACTIVE_SIGNAL` + `radio_notification_cb(INACTIVE)` remain.

## select order + idle BLE disconnect (resolved: cause was the dongle)

`prx_task` runs `select(BLE_INACTIVE_SIGNAL.wait(), session.next_event())` with
BLE_INACTIVE polled first, so `request_window` (open an RX window) wins over
`next_event` processing. This prevents ESB co-channel noise (0x83 invalid
frames from a nearby powered dongle on the same addr/pipe) from starving
`request_window` and dropping right-half snapshots (drops 17% -> ~0%).

**Earlier hypothesis (now refuted):** BLE_INACTIVE-first opens a window on
*every* conn event (30ms), which was believed to contend with BLE conn events
-> conn-event loss -> supervision timeout -> idle disconnect.

**Verified wrong on hardware:** with the co-channel dongle removed, a 12+ minute
run opening a window every 30ms (heavy typing, ~290 snapshots, seq contiguous)
produced **0 BLE disconnects**. Window cadence is independent of HID traffic,
so if windows contended they would disconnect under load too — they don't. The
earlier idle disconnect was the **co-channel dongle's ESB transmissions jamming
the left's BLE conn events** (2.4GHz co-channel RF interference), not the parked
windows. The BLE_INACTIVE-first select order is kept: it costs nothing (no
disconnect confirmed) and stays robust to a future dongle.

`request_window` occasionally returns `ret=-35` (~1-2%). Identified:
`-NRF_EAGAIN` ("the session is not IDLE", per `mpsl_timeslot.h`) — a benign race
between our `request_outstanding` guard and MPSL's actual session state. The
next 30ms window recovers; no packet loss, no disconnect. Not worth fixing.

## Still open

- **Host (Mac) rejects conn 15ms / latency 30, keeps 30ms / latency 0.** With
  latency 0 the left must RX every conn event -> BLE costs ~550uA idle. Getting
  Mac to accept a non-zero slave latency would let the left sleep through conn
  events and cut BLE idle to ~tens of uA. **Biggest remaining idle-power lever.**
- **Co-channel dongle** (only relevant if a dongle runs alongside the halves):
  use distinct ESB addresses (base/prefix) so the PRX hardware address-filter
  drops the dongle's packets instead of relying on unplug. This stops the left
  *receiving* dongle frames, but does not eliminate RF co-channel energy; BLE
  frequency-hopping makes occasional jams tolerable (supervision timeout needs
  ~6s of consecutive misses). Verify with the dongle powered.
