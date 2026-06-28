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

## Slave-latency investigation (closed: unreachable on macOS)

Hypothesis: request non-zero slave latency so the left can sleep through BLE
conn events -> cut BLE idle from ~550uA (latency 0) to ~tens of uA. Tested both
mechanisms on hardware; both blocked:

1. **L2CAP Connection Parameter Update (signaling code 0x12).** Sent after
   encryption (interval 15-30ms, latency 6, timeout 5s). Mac responds `0`
   (accepted) but applies **no update** — no LE Connection Update Complete event
   fires; interval stays 30ms, latency stays 0. macOS accepts the request then
   ignores the requested latency (enforces latency 0 for HID).
2. **LL procedure (HCI LE_Connection_Update / `LeConnUpdate`).** Linker error:
   `sdc_hci_cmd_le_conn_update` exists only in the **central** and **multirole**
   SoftDevice Controller libraries, not the **peripheral** one the left links.
   Nordic gates LE Connection Update as a master/central command; the peripheral
   build only exposes the L2CAP path. Switching to multirole is inappropriate
   (peripheral-only device) and won't fit — multirole lib ~697KB vs current
   ~104KB total flash on the nRF52833's 512KB.

**Conclusion: macOS will not grant slave latency via any available mechanism.**
BLE idle (~550uA, RX every conn event) is host-capped. Total idle ~0.8mA is
mostly BLE; the parked ESB windows piggyback on the already-scheduled BLE wake
(nearly free timing). Decoupling ESB wake to an RTC can only cut the small ESB
share (~0.3mA) and would worsen first-key latency — not worth it. **The scheme
is at its practical idle-power floor for a macOS-connected HID keyboard.**

Follow-up win: a redundant always-on HFCLK task (`hfclk_task`, which held an
MPSL HFCLK guard forever via `pending()`) was keeping the 16MHz HFXO running
continuously — pure idle drain, since MPSL already provides HFXO on demand per
radio event. Removed from the LEFT in 7ec5342 — but the symmetric fix was
missed on the RIGHT half (the half meant to deep-sleep), so it still drew ~1mA
idle and the "beats RMK on power" claim did not hold. An independent review
caught it; the right's `hfclk_task` is now also removed (af32ae6). Exact idle
numbers still pending PPK2 — but this was the largest single idle-power leak.

Cadence-probe aside (corrects the self-trigger reasoning above): a window
counter showed ~66.7 BLE_INACTIVE signals/sec at a 30ms conn interval = **two
INACTIVE edges per conn event** (the BLE radio event + the ESB parked window
each fire their own INACTIVE). So the ESB window's INACTIVE is **not** merged/
skipped as the "why the naive reading was wrong" section claimed. There is still
no self-trigger runaway — but the protection is the `request_outstanding` guard
(`request_window` returns early while a window is active/outstanding), not
notification merging. Conclusion unchanged; only the mechanism was mis-stated.

## Adaptive fast-cadence — right-key latency fixed (beats RMK)

The parked-only design (one RX window per BLE conn event, ~30ms) made right-key
latency ~46ms: a slow ESB hop (right waits up to 30ms for the next window) plus
a ~28ms receive→HID gap (a packet received early in the BLE gap must wait for
the *next* conn event to be forwarded). Both are artifacts of the 30ms cadence,
not inherent to ESB.

Fix (6328eb1 + d25677e): **budget-driven callback chaining**. While frames
arrive, the PRX slot-end callback chains fast NORMAL windows (~8ms) — the proven
continuous-PRX primitive, so it's MPSL-safe (no re-trigger of the earlier
shift-late stall). A frame refills a budget; when it exhausts (~88ms of no
traffic) the session falls back to parked. `prx_task` is unchanged; all logic is
callback-context atomics (no app time source). The ACK hint refreshes on frame
receipt (before the hint is built) so the right's `bounded_wait` sees the fast
period with no one-ACK lag.

### Measured latency (hardware, 2026-06-28)

Both legs measured independently on each half's RTT (active-hold 240ms, fast
cadence 8ms; `window_us` reverted to `in_slot_match` 530µs — see note below):

**HID leg** (left `hid_leg` = receive→next-BLE-INACTIVE, 695 samples):
median **~10ms**, range 0–15ms, bimodal (~50% at 0–9ms, ~46% at 10–12ms).

**ESB leg** (right `lat` = keypress→ACK, 466 valid sends, left ACKing):

| attempts | share | ESB latency |
|---|---|---|
| att=1 (first try) | 75% | **~2.8ms** (2777–2808µs) |
| att=2 (one retry) | 24% | **~6ms** (cluster 5.8–6.1ms) |
| att=3 (two retries) | 1% | ~9–11ms |

**Total right-key latency (steady-state)** = ESB + HID:

| | total |
|---|---|
| median (att=1) | **~13ms** |
| retry tail (att=2, 24%) | ~16ms |
| att=3 (1%) | ~20ms |
| transition (first key after >240ms idle) | ~30ms (parked wake-up) |

vs **RMK pure-BLE split (estimated, not measured)**: median ~20ms, worst ~40ms
(two independent BLE hops both missing their conn events). So steady-state this
design beats RMK on both median (13 vs 20) and worst-case tail (~20 vs ~40);
the only weaker spot is the parked-wake-up transition (~30ms vs RMK's no-penalty
~20ms, but still < RMK's ~40ms worst).

**Retry analysis:** ~25% of sends retry once (att=2). Root cause is timing
misalignment — the right's RTC drifts vs the left's TIMER0, and `max_wait`=2ms
makes ~75% of sends fall back to an immediate (unaligned) TX that misses the
~530µs RX window ~1/3 of the time. An experiment advertising the full
`slot_length` (1500µs) as `window_us` instead of `in_slot_match` (530µs) was
**neutral** (~25% either way — the retry is alignment-bound, not window-width-
bound); reverted (880610c → 71e18f3). The real lever to tighten the tail would
be raising the right's `max_wait` (2→~6ms, waits for alignment instead of
falling back) at the cost of ~1ms median — deferred.

0 BLE disconnects / errors across heavy typing. Idle stays low-power (parked
~30ms); the fast cadence only runs while typing.

### Held-key keepalive + link-loss

Held-key keepalive (right half): the matrix doesn't change during a hold, so
without keepalives the link-loss watchdog (500ms) released the key mid-hold and
OS auto-repeat died after ~2 chars. The right re-sends the current snapshot
every 100ms while a key is held (was 150ms — widened to ~5x the watchdog margin
under interference), keeping the left's `last_seen` fresh. The link-loss
watchdog moved to a periodic `link_loss_task` (the inline `hid_task` check
never re-ran when idle).

Note: this fixes *ESB* latency. The BLE-idle power floor (~550uA, macOS-enforced
latency 0 — see the slave-latency section above) is unchanged; adaptive cadence
doesn't sleep BLE, it just stops the ESB from inheriting the 30ms cadence.

## Still open
- **Co-channel dongle** (only relevant if a dongle runs alongside the halves):
  use distinct ESB addresses (base/prefix) so the PRX hardware address-filter
  drops the dongle's packets instead of relying on unplug. This stops the left
  *receiving* dongle frames, but does not eliminate RF co-channel energy; BLE
  frequency-hopping makes occasional jams tolerable (supervision timeout needs
  ~6s of consecutive misses). Verify with the dongle powered.
