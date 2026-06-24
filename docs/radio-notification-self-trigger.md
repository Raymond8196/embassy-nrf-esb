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

## Not yet verified (Step 3)

- Whether parked windows actually **receive** right-half packets — the probe only
  counts (`request_window`), does not call `try_next_event`.
- Right-half PTX is currently `PhaseLocked` to the old continuous rhythm; parked
  windows sit in BLE gaps on a different cadence, so packets likely miss until
  PTX timing is adapted (disable `PhaseLocked`, or switch to explicit
  window/backoff). This is the paired change called out in the review.
