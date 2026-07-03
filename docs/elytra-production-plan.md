# Elytra Production ESB Plan: 2-Week Sprint

Created: 2026-06-23

Status note, 2026-07-03: this remains the product target document for G1, G2,
and manual runtime switching. It is not the current status source; use
`current-status.md` for the latest verified state and next actions. The
radio-notification parked PRX and adaptive-cadence work became the active G1
hardening path after this sprint plan was written.

Goal: production-grade ESB on Elytra split keyboard within 2 weeks. The sprint
prioritizes a stable G1 daily-driver path, then a usable G2 dongle path, then
runtime switching only after both paths are independently verified. This becomes
the foundation for multi-split.

## 1. Goals

| # | Mode | Topology | Radio | Use case |
|---|------|----------|-------|----------|
| G1 | ESB+BLE | Right PTX → Left PRX + BLE HID → PC | MPSL coexistence | Replace pure-BLE; daily driver |
| G2 | Pure 2.4G | Both halves PTX → Dongle PRX + USB HID | Exclusive (dongle), MPSL (halves) | Lowest latency; competitive/gaming |

## 2. Sprint cut lines

The sprint has explicit release-candidate tiers. Later tiers must not block an
earlier tier from being declared usable.

| Tier | Scope | Required outcome |
|------|-------|------------------|
| RC1 | G1 ESB+BLE daily driver | Left/right keyboard works through BLE HID, survives reconnects, and passes the G1 soak gate. |
| RC2 | G2 pure 2.4G manual mode | Both halves can send to the dongle with compile-time or key-combo mode selection; dongle emits USB HID. |
| RC3 | Runtime mode switch | Manual G1↔G2 switch is recoverable, does not leave stuck keys, and falls back to G1 on failure. |
| Deferred | Automatic dongle detection | Only starts after RC1-RC3 are stable and a reliable detection mechanism is defined. |

**Important scope rule:** automatic dongle detection is not a two-week blocker.
The first runtime switch target is manual, explicit, and recoverable. A dongle
that stays PRX-only cannot proactively advertise presence; auto-detection must
use active probe/ACK, hardware presence, or a later dongle downlink/beacon
design.

## 3. Architecture decision: keep MPSL always on keyboard halves

### The problem

`bind_interrupts!` is compile-time. RADIO/TIMER0/RTC0 vectors go to MPSL's
`HighPrioInterruptHandler` in the current Elytra firmware. Exclusive-mode ESB
needs `EsbPtx::on_radio_interrupt()` on the RADIO vector. Switching vectors at
runtime requires an `AtomicU8` ISR dispatcher (RMK Phase 4.1 PoC pattern) that
is **not implemented in this repo**.

### The solution

**Keyboard halves keep MPSL alive in both modes.** The mode switch is an ESB
role/config change within MPSL, not a RADIO vector rebinding:

| | Left half | Right half | Dongle |
|---|-----------|------------|--------|
| **G1** | MPSL PRX session + BLE SDC | MPSL PTX event session → left | (not used) |
| **G2** | MPSL PTX session → dongle (BLE disconnected or quiesced; policy decided during D6) | MPSL PTX event session → dongle | Exclusive PRX (USB HID) |

Mode switch = close one MPSL session + open another + apply the selected BLE
policy. No ISR vector changes, no HFCLK handoff, no MPSL teardown.

**Dongle is pure exclusive mode** (no MPSL, no BLE): simplest firmware, lowest
latency, USB-only. ESB radio packets are identical regardless of which side
manages the radio, so MPSL-PTX ↔ exclusive-PRX communication works natively.

### Why not exclusive mode on keyboard halves for G2?

Exclusive mode would give ~100-200µs lower per-slot overhead, but:
- Requires AtomicU8 ISR dispatch (not yet implemented, ~1 week effort)
- Requires full MPSL teardown/rebuild (mpsl.run() is `-> !`, no clean stop)
- HFCLK handoff complexity
- **Not worth the risk in a 2-week sprint.** MPSL without BLE contention
  approaches exclusive-mode latency closely enough for keyboard use.

Deferred to post-sprint if latency measurements show MPSL overhead is the
bottleneck.

## 4. Early risk register

These are listed before the task breakdown because they directly affect the
development order. Every runtime-switch task must be checked against this list.

| Risk | Why it matters | Sprint decision / mitigation |
|------|----------------|------------------------------|
| Right half misses the final G1 switch command | G1 is mostly right PTX → left PRX; the switch command likely travels through ACK payload. A lost final command can strand the halves in different modes. | RC3 requires a two-phase switch protocol with `PREPARE`, repeated ACKed command, epoch, timeout, and fallback to G1. |
| PRX→PTX role switch leaves MPSL state behind | Left half changes from MPSL PRX in G1 to MPSL PTX in G2. Session drop/open must clean RADIO/TIMER0 state reliably. | Verify compile-time/manual single-role G2 before runtime switching; stress 20 PRX→PTX→PRX cycles before calling RC3 usable. |
| BLE "pause" is ambiguous | Keeping BLE connected preserves quick return but still creates connection-event contention. Disconnecting gives cleaner G2 but makes G1 return slower. | D6 must choose and document the G2 BLE policy: keep connected/quiesce HID, disconnect, or advertising-only. |
| Stuck modifiers/keys across switch | A press in one mode and release in another can leave Shift/Ctrl or a key stuck on the host. | Switch protocol sends all-keys-up before mode change and first packet after switch is a full matrix snapshot with a new epoch. |
| Sequence/dedup state crosses modes incorrectly | Old delayed packets or reused sequence numbers can be mistaken for valid new-mode events. | Clear per-device `SequenceTracker` on mode switch or include a switch epoch in the transport-level validation path. |
| Dongle presence is not directly observable | A PRX-only dongle cannot send a heartbeat before a keyboard half transmits. | Manual key combo is the primary RC3 trigger; auto-detect is deferred unless active probe/ACK is proven reliable. |
| Dongle keymap is larger than "USB HID merge" | Moving keymap processing to dongle moves layer/modifier state and raw matrix/event semantics there too. | RC2 may start with a minimal keymap, but the plan must keep raw matrix snapshot/event format consistent between G1 and G2. |
| MPSL PTX → exclusive PRX compatibility is assumed | Existing exclusive examples do not prove Elytra MPSL PTX can talk to a new exclusive dongle PRX under the G2 config. | D4 must explicitly test Elytra/MPSL PTX against the dongle PRX before full G2 integration. |
| Latency and key-loss metrics are not measurable by hand | p99 latency and <0.01% loss require repeatable instrumentation. | V0/D10/D12 must define the measurement method before quoting pass/fail numbers. |

## 5. Role mapping per device

### Left half (the complex one — switches role)

```
G1: PRX (pipe 0, from right) + BLE peripheral (HID to PC) + USB CDC (debug)
    Address set A: right→left on pipe 0
G2: PTX (pipe 0, to dongle) + selected BLE policy + USB CDC (debug)
    Address set B: left→dongle on pipe 0

Runtime switch trigger: user key combo first; auto dongle detection deferred.
```

### Right half (always PTX, switches target)

```
G1: PTX event session (pipe 0, to left)
    Address set A: right→left on pipe 0
G2: PTX event session (pipe 0, to dongle)
    Address set B: right→dongle on pipe 1

Runtime switch trigger: two-phase sync with left half over the existing G1 link
before switching. If sync fails, stay or return to G1.
```

### Dongle (G2 only, always exclusive PRX)

```
PRX exclusive mode, pipes 0+1 (left=pipe0, right=pipe1)
USB HID (full keymap processing)
No MPSL, no BLE, no SDC.
```

## 6. Runtime switch protocol target

RC3 uses a conservative manual switch flow:

1. User presses the mode-switch key combo.
2. Left sends repeated `SWITCH_PREPARE(mode=G2, epoch)` messages to right via
   G1 ACK payloads.
3. Right acknowledges by sending a frame with the same epoch while still in G1.
4. Both sides send or synthesize all-keys-up and clear host-visible HID state.
5. Both sides close their current MPSL sessions and open the G2 sessions with
   address set B.
6. First G2 packet from each half is a full matrix snapshot tagged with the new
   epoch.
7. If any side cannot confirm G2 within the timeout, it returns to G1 and clears
   key state again.

The G2→G1 path follows the same pattern in reverse. Automatic switching from
dongle insertion/removal is a later extension, not the first implementation.

## 7. Deliverables and task breakdown

### Week 1: RC1 G1 production hardening + RC2 dongle foundation

#### D1-D2 (Mon-Tue evening): G1 baseline verification + hardening

- [ ] **V0: Baseline regression** (tonight, ~30 min)
  - Flash current `left_central` + `main` (right) to Elytra
  - Verify: right half key events arrive at left half
  - Verify: BLE HID to phone/PC works
  - Verify: USB CDC stats look normal (rx count, ACK rate, no errors)
  - Verify: 10-minute continuous typing, no stuck keys, no panic
  - **Record baseline metrics**: OK rate, ACK rate, retry/max-attempt count,
    duplicate drops, and the chosen latency measurement method

- [ ] **D1: Production hardening items** (Mon evening, code-only)
  - Audit `left_central.rs` error handling: all session open failures must
    degrade gracefully (currently some panic)
  - Add BLE disconnect/reconnect resilience: verify G1 survives phone
    walking out of range and coming back
  - Verify transport ACK dedup under retransmit: rapid key bursts should
    not produce duplicate events after RMK-layer `SequenceTracker`
  - Add defmt panic handler (not panic-probe) for production: must not
    hang on RTT if defmt isn't connected
  - Add explicit all-keys-up / HID clear path for disconnect, panic recovery,
    and future mode switches

- [ ] **D2: Right half hardening** (Tue evening, code + hardware)
  - Matrix scan: verify all rows (P0.03 issue documented in bring-up —
    confirm rows 0-3 now report, not just row 4)
  - Add key debounce to the event source if not already present
  - Verify ESB retransmit: power off left half briefly, right half should
    retry; power back on, link recovers within 1-2 slots
  - Verify first packet after reconnect is a full matrix snapshot so stale
    state cannot survive link loss

#### D3-D4 (Wed-Thu evening): Dongle firmware (G2 PRX exclusive)

- [ ] **D3: Dongle PRX exclusive firmware** (Wed evening, code-only)
  - New `boards/elytra/src/bin/dongle_central.rs` (or example)
  - Uses `EsbPrx::new()` (exclusive mode, not MPSL)
  - Multi-pipe: pipe 0 = left half, pipe 1 = right half
  - USB HID: start with a minimal fixed keymap, then grow toward full keymap
    once transport and multi-pipe routing are stable
  - Transport layer: `accept_bound_frame` per pipe, merge into one keymap
  - HFCLK: direct `CLOCK.tasks_hfclkstart()` (no MPSL)
  - No BLE, no SDC, no MPSL code linked
  - Track per-device counters: rx, duplicate drop, decode error, binding
    mismatch, max-attempt/ACK information if available

- [ ] **D4: Dongle bring-up** (Thu evening, hardware)
  - Flash dongle firmware to nRF52840 dongle
  - USB HID enumerate on PC
  - Test with exclusive PTX example first (`ptx_split_peripheral`), verify
    basic RX on pipe 0
  - Test the real risky link: Elytra MPSL PTX event session → exclusive dongle
    PRX, with the same address/pipe config planned for G2
  - Test multi-pipe: two PTX senders, verify both pipes received and bound to
    the correct device ids

#### D5 (Fri evening): Right half G2 mode

- [ ] **D5: Right half dual-mode** (Fri evening, code + hardware)
  - Add dongle address config to right half
  - Add manual mode selection: compile-time flag first, then key-combo switch
    only after compile-time G2 works
  - Use `open_event_session` with left-half address (G1) vs dongle address (G2)
  - Test: right half → dongle, verify key events arrive over USB HID

#### D6-D7 (Sat-Sun): Left half G2 mode + first integration

- [ ] **D6: Left half PTX mode** (Sat, code + hardware)
  - Left half gains PTX capability for G2 (alongside existing PRX for G1)
  - Manual mode only at first: close PRX session → open PTX session → apply
    the selected BLE policy
  - Decide and document the G2 BLE policy:
    - keep BLE connected but suppress HID,
    - actively disconnect BLE and restart advertising on G2→G1,
    - or another measured policy with clear latency/recovery tradeoff
  - This is the hardest single task: PRX→PTX role change within MPSL
  - Verify: left half key events arrive at dongle over USB HID

- [ ] **D7: Full G2 integration** (Sun, hardware)
  - Both halves in G2 mode → dongle
  - Full keyboard typing test: all keys from both halves arrive correctly
  - Verify: no cross-pipe contamination, no missing keys
  - Record G2 latency metrics for comparison with G1

### Week 2: RC3 manual runtime switching + production validation

#### D8-D9 (Mon-Tue evening): Manual runtime mode switch

- [ ] **D8: Manual two-phase switch** (Mon evening, code + hardware)
  - Trigger: user key combo, not auto dongle detection
  - Implement `SWITCH_PREPARE(mode, epoch)` over G1 ACK payloads, repeated
    until right confirms or timeout expires
  - On confirmed switch: clear HID/all-keys-up, close current sessions, open
    target sessions, and send full matrix snapshot in the new mode
  - On timeout or dongle ACK/probe failure: return to G1 and clear HID again
  - Clear or epoch-tag transport dedup state on each mode switch

- [ ] **D9: Mode-switch reliability** (Tue evening, hardware)
  - Stress: manual G1↔G2 switch 20 times, verify recovery each time
  - Stress: switch while typing (mid-keystream), verify no lost keys beyond
    the switch transition (~100ms acceptable)
  - Verify: BLE reconnects cleanly when returning to G1
  - Verify: missed switch command or dongle unavailable returns both halves to
    G1 within the timeout
  - Optional only: try active dongle probe as an auto-detect spike if manual
    switching is already stable

#### D10-D11 (Wed-Thu evening): Latency + power optimization

- [ ] **D10: G2 latency tuning** (Wed evening, hardware)
  - Measure: G2 end-to-end latency (key→USB HID report) vs G1
  - Use a repeatable measurement path before claiming p99: GPIO pulse +
    logic analyzer, host HID timestamp script, or firmware sequence/timestamp
    counters with documented limitations
  - Tune: slot length, in-slot match time, poll interval, packets-per-slot
  - Target: G2 p99 latency < 5ms (matrix scan 2ms + ESB 2ms + USB 1ms)
  - Compare with G1 to quantify the improvement

- [ ] **D11: Power measurement** (Thu evening, hardware with ammeter)
  - Measure: right half sleep current (no keys, 30s average)
  - Measure: left half G1 idle current (BLE connected, no keys)
  - Measure: left half G2 idle current under the selected BLE policy
  - Target: right half < 500µA idle (current baseline TBD)
  - Optimize: PTX event session should request slots only on key events,
    not continuously (already the case via `open_event_session`)

#### D12-D13 (Fri-Sat): Long-run + edge cases

- [ ] **D12: 8-hour soak test** (Fri evening → overnight)
  - G1 mode: both halves, BLE to phone/PC, deterministic typing automation or
    matrix exerciser with host-side HID verifier
  - Pass criteria: zero panics, <0.01% key loss, BLE stays connected
  - Check: memory leak (stack high-water mark via `thread-model` canary)

- [ ] **D13: G2 soak test + edge cases** (Sat)
  - G2 mode: both halves → dongle, deterministic typing automation or matrix
    exerciser with host-side HID verifier
  - Edge: low battery on one half (brownout recovery)
  - Edge: dongle replug mid-typing
  - Edge: one half power-cycled while other keeps typing

#### D14 (Sun): Buffer + documentation

- [ ] **D14: Final validation + docs** (Sun)
  - Full regression: G1 30min + G2 30min + mode switch 10 cycles
  - Update README with Elytra production status
  - Record metrics: latency, power, reliability for both modes
  - Update `docs/current-status.md` and README status
  - Explicitly label achieved tier: RC1, RC2, or RC3. Do not describe deferred
    auto-detection as complete unless it has its own verification record.

## 8. Verification matrix

| Test | Mode | Duration | Pass criteria |
|------|------|----------|---------------|
| G1 basic typing | G1 | 30 min | All keys arrive once, no dups |
| G1 BLE resilience | G1 | 10 min | BLE disconnect/reconnect 5x, no ESB impact |
| G1 soak | G1 | 8 hr | Zero panic, <0.01% loss |
| G1 stuck-key guard | G1 | per case | Disconnect/reconnect, panic/reset, and link loss all end with all keys released |
| G2 basic typing | G2 | 30 min | All keys from both halves via USB HID |
| G2 multi-pipe | G2 | 10 min | No cross-pipe contamination |
| MPSL PTX ↔ exclusive PRX | G2 | 10 min | Elytra MPSL PTX talks to exclusive dongle PRX with production G2 addresses |
| G2 latency | G2 | measured | p99 < 5ms end-to-end |
| Manual mode switch | G1↔G2 | 20 cycles | Recovery < 500ms, no stuck keys, fallback to G1 on timeout |
| Mode switch mid-type | G1↔G2 | 10 cycles | No stuck modifiers; transition may drop at most the active switch-window key event |
| Missed switch command | G1↔G2 | 10 cycles | One side missing `SWITCH_PREPARE` or dongle ACK/probe failure returns both halves to G1 |
| Low battery | both | per case | Brownout recovery, no corrupt state |
| Right half retransmit | G1 | 5 min | Power off/on left, link recovers < 2 slots |
| Dongle replug | G2 | 10 cycles | Both halves resync within 1s |
| Auto dongle detection | deferred | spike only | Must not be required for RC1-RC3 |

## 9. Technical risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| PRX→PTX role switch within MPSL fails (state leak) | Medium | High | Start with compile-time switch; add runtime only after both roles verified independently |
| Dongle exclusive PRX can't talk to MPSL PTX (timing mismatch) | Low | Critical | Verify early (D4) with real Elytra/MPSL PTX against dongle PRX before building full G2 assumptions |
| Right half misses switch command | Medium | High | Use repeated `SWITCH_PREPARE(epoch)` plus right-side confirmation and timeout fallback to G1 |
| Mode switch loses keys or leaves stuck modifiers | Medium | High | Send all-keys-up before switch; first post-switch packet is a full matrix snapshot; clear HID state on timeout |
| BLE doesn't resume cleanly after G2→G1 | Medium | Medium | Choose explicit G2 BLE policy in D6; test D9 thoroughly; fall back to hard BLE reconnect if soft resume fails |
| Matrix row P1.03 still broken | Low | High | Already documented; if still broken, hardware fix needed (can't software around it) |
| MPSL PTX event session latency in G2 is too high | Medium | Medium | Tune slot params (D10); if insufficient, escalate to exclusive-mode switch (post-sprint) |
| Auto dongle detection proves unreliable | High | Medium | Keep manual key-combo switch as RC3 target; defer auto-detect unless active probe/ACK is proven |
| Latency/loss claims are not reproducible | Medium | High | Define instrumentation in V0/D10/D12 before using p99 or loss-rate pass criteria |

## 10. What this unblocks next: multi-split

After this sprint:
- **Transport layer proven** in both MPSL and exclusive modes with real keyboard traffic
- **Multi-pipe PRX** proven on dongle (pipe 0+1) — extends naturally to pipe 2-7 for 3-7 peripherals
- **Manual runtime mode switch** infrastructure exists if RC3 lands — adding a 3rd mode (multi-split) reuses the session-close/open pattern
- **Production validation methodology** established (soak test, latency measurement, power measurement) — reusable for multi-split qualification

Multi-split specific work (post-sprint):
- Address assignment / pairing protocol (currently static binding)
- Channel hopping (if interference testing shows need)
- Dongle keymap merge for >2 peripherals
- Automatic dongle/mode discovery if manual switching is not enough
