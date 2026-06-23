# Elytra Production ESB Plan: 2-Week Sprint

Created: 2026-06-23

Goal: production-grade ESB on Elytra split keyboard within 2 weeks, with two
radio modes switchable at runtime, as the foundation for multi-split.

## 1. Goals

| # | Mode | Topology | Radio | Use case |
|---|------|----------|-------|----------|
| G1 | ESB+BLE | Right PTX → Left PRX + BLE HID → PC | MPSL coexistence | Replace pure-BLE; daily driver |
| G2 | Pure 2.4G | Both halves PTX → Dongle PRX + USB HID | Exclusive (dongle), MPSL (halves) | Lowest latency; competitive/gaming |

**Runtime switch**: keyboard halves detect dongle presence and switch between
G1 and G2 without reflash. Dongle is G2-only (USB HID, exclusive PRX).

## 2. Architecture decision: keep MPSL always on keyboard halves

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
| **G2** | MPSL PTX poll session → dongle (BLE paused) | MPSL PTX event session → dongle | Exclusive PRX (USB HID) |

Mode switch = close one MPSL session + open another + pause/resume BLE. No ISR
vector changes, no HFCLK handoff, no MPSL teardown.

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

## 3. Role mapping per device

### Left half (the complex one — switches role)

```
G1: PRX (pipe 0, from right) + BLE peripheral (HID to PC) + USB CDC (debug)
    Address set A: right→left on pipe 0
G2: PTX (pipe 0, to dongle) + BLE paused + USB CDC (debug)
    Address set B: left→dongle on pipe 0

Runtime switch trigger: dongle USB detect or user key combo.
```

### Right half (always PTX, switches target)

```
G1: PTX event session (pipe 0, to left)
    Address set A: right→left on pipe 0
G2: PTX event session (pipe 0, to dongle)
    Address set B: right→dongle on pipe 1

Runtime switch trigger: sync with left half via existing left↔right link
(before switching), or independent dongle detection.
```

### Dongle (G2 only, always exclusive PRX)

```
PRX exclusive mode, pipes 0+1 (left=pipe0, right=pipe1)
USB HID (full keymap processing)
No MPSL, no BLE, no SDC.
```

## 4. Deliverables and task breakdown

### Week 1: G1 production hardening + dongle foundation

#### D1-D2 (Mon-Tue evening): G1 baseline verification + hardening

- [ ] **V0: Baseline regression** (tonight, ~30 min)
  - Flash current `left_central` + `main` (right) to Elytra
  - Verify: right half key events arrive at left half
  - Verify: BLE HID to phone/PC works
  - Verify: USB CDC stats look normal (rx count, ACK rate, no errors)
  - Verify: 10-minute continuous typing, no stuck keys, no panic
  - **Record baseline metrics**: p50/p99 latency, OK rate, ACK rate

- [ ] **D1: Production hardening items** (Mon evening, code-only)
  - Audit `left_central.rs` error handling: all session open failures must
    degrade gracefully (currently some panic)
  - Add BLE disconnect/reconnect resilience: verify G1 survives phone
    walking out of range and coming back
  - Verify transport ACK dedup under retransmit: rapid key bursts should
    not produce duplicate events after RMK-layer `SequenceTracker`
  - Add defmt panic handler (not panic-probe) for production: must not
    hang on RTT if defmt isn't connected

- [ ] **D2: Right half hardening** (Tue evening, code + hardware)
  - Matrix scan: verify all rows (P0.03 issue documented in bring-up —
    confirm rows 0-3 now report, not just row 4)
  - Add key debounce to the event source if not already present
  - Verify ESB retransmit: power off left half briefly, right half should
    retry; power back on, link recovers within 1-2 slots

#### D3-D4 (Wed-Thu evening): Dongle firmware (G2 PRX exclusive)

- [ ] **D3: Dongle PRX exclusive firmware** (Wed evening, code-only)
  - New `boards/elytra/src/bin/dongle_central.rs` (or example)
  - Uses `EsbPrx::new()` (exclusive mode, not MPSL)
  - Multi-pipe: pipe 0 = left half, pipe 1 = right half
  - USB HID: full keymap merge from both halves
  - Transport layer: `accept_bound_frame` per pipe, merge into one keymap
  - HFCLK: direct `CLOCK.tasks_hfclkstart()` (no MPSL)
  - No BLE, no SDC, no MPSL code linked

- [ ] **D4: Dongle bring-up** (Thu evening, hardware)
  - Flash dongle firmware to nRF52840 dongle
  - USB HID enumerate on PC
  - Test with exclusive PTX example first (`ptx_split_peripheral`), verify
    basic RX on pipe 0
  - Test multi-pipe: two PTX senders, verify both pipes received

#### D5 (Fri evening): Right half G2 mode

- [ ] **D5: Right half dual-mode** (Fri evening, code + hardware)
  - Add dongle address config to right half
  - Add runtime switch: `open_event_session` with left-half address (G1)
    vs dongle address (G2)
  - Trigger: compile-time flag first (verify correctness), then runtime
    switch via key combo
  - Test: right half → dongle, verify key events arrive over USB HID

#### D6-D7 (Sat-Sun): Left half G2 mode + first integration

- [ ] **D6: Left half PTX mode** (Sat, code + hardware)
  - Left half gains PTX capability for G2 (alongside existing PRX for G1)
  - Mode switch: close PRX session → open PTX poll session → pause BLE
  - This is the hardest single task: PRX→PTX role change within MPSL
  - Verify: left half key events arrive at dongle over USB HID

- [ ] **D7: Full G2 integration** (Sun, hardware)
  - Both halves in G2 mode → dongle
  - Full keyboard typing test: all keys from both halves arrive correctly
  - Verify: no cross-pipe contamination, no missing keys
  - Record G2 latency metrics for comparison with G1

### Week 2: Runtime switching + production validation

#### D8-D9 (Mon-Tue evening): Runtime mode switch

- [ ] **D8: Dongle detection + auto-switch** (Mon evening, code + hardware)
  - Detect dongle presence: USB VBUS on left half (if wired), or heartbeat
    packet from dongle on ESB
  - On dongle connect: switch both halves to G2 mode
  - On dongle disconnect: switch back to G1 mode
  - Coordinate left↔right: left half tells right half to switch via the
    existing G1 link before tearing it down

- [ ] **D9: Mode-switch reliability** (Tue evening, hardware)
  - Stress: rapid plug/unplug dongle 20 times, verify recovery each time
  - Stress: switch while typing (mid-keystream), verify no lost keys beyond
    the switch transition (~100ms acceptable)
  - Verify: BLE reconnects cleanly when returning to G1

#### D10-D11 (Wed-Thu evening): Latency + power optimization

- [ ] **D10: G2 latency tuning** (Wed evening, hardware)
  - Measure: G2 end-to-end latency (key→USB HID report) vs G1
  - Tune: slot length, in-slot match time, poll interval, packets-per-slot
  - Target: G2 p99 latency < 5ms (matrix scan 2ms + ESB 2ms + USB 1ms)
  - Compare with G1 to quantify the improvement

- [ ] **D11: Power measurement** (Thu evening, hardware with ammeter)
  - Measure: right half sleep current (no keys, 30s average)
  - Measure: left half G1 idle current (BLE connected, no keys)
  - Measure: left half G2 idle current (BLE paused, no keys)
  - Target: right half < 500µA idle (current baseline TBD)
  - Optimize: PTX event session should request slots only on key events,
    not continuously (already the case via `open_event_session`)

#### D12-D13 (Fri-Sat): Long-run + edge cases

- [ ] **D12: 8-hour soak test** (Fri evening → overnight)
  - G1 mode: both halves, BLE to phone, overnight typing automation
  - Pass criteria: zero panics, <0.01% key loss, BLE stays connected
  - Check: memory leak (stack high-water mark via `thread-model` canary)

- [ ] **D13: G2 soak test + edge cases** (Sat)
  - G2 mode: both halves → dongle, overnight typing automation
  - Edge: low battery on one half (brownout recovery)
  - Edge: dongle replug mid-typing
  - Edge: one half power-cycled while other keeps typing

#### D14 (Sun): Buffer + documentation

- [ ] **D14: Final validation + docs** (Sun)
  - Full regression: G1 30min + G2 30min + mode switch 10 cycles
  - Update README with Elytra production status
  - Record metrics: latency, power, reliability for both modes
  - Update `docs/roadmap-to-9.md` scores

## 5. Verification matrix

| Test | Mode | Duration | Pass criteria |
|------|------|----------|---------------|
| G1 basic typing | G1 | 30 min | All keys arrive once, no dups |
| G1 BLE resilience | G1 | 10 min | BLE disconnect/reconnect 5x, no ESB impact |
| G1 soak | G1 | 8 hr | Zero panic, <0.01% loss |
| G2 basic typing | G2 | 30 min | All keys from both halves via USB HID |
| G2 multi-pipe | G2 | 10 min | No cross-pipe contamination |
| G2 latency | G2 | measured | p99 < 5ms end-to-end |
| Mode switch | G1↔G2 | 20 cycles | Recovery < 500ms, no stuck keys |
| Mode switch mid-type | G1↔G2 | 10 cycles | ≤1 key lost per transition |
| Low battery | both | per case | Brownout recovery, no corrupt state |
| Right half retransmit | G1 | 5 min | Power off/on left, link recovers < 2 slots |
| Dongle replug | G2 | 10 cycles | Both halves resync within 1s |

## 6. Technical risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| PRX→PTX role switch within MPSL fails (state leak) | Medium | High | Start with compile-time switch; add runtime only after both roles verified independently |
| Dongle exclusive PRX can't talk to MPSL PTX (timing mismatch) | Low | Critical | Verify early (D4) with `ptx_split_peripheral` example before building full dongle firmware |
| Mode switch loses keys (user notices transition) | Medium | Medium | Accept ≤1 key loss; buffer last key and resend after switch |
| BLE doesn't resume cleanly after G2→G1 | Medium | Medium | Test D9 thoroughly; fall back to hard BLE reconnect if soft resume fails |
| Matrix row P1.03 still broken | Low | High | Already documented; if still broken, hardware fix needed (can't software around it) |
| MPSL PTX event session latency in G2 is too high | Medium | Medium | Tune slot params (D10); if insufficient, escalate to exclusive-mode switch (post-sprint) |

## 7. What this unblocks next: multi-split

After this sprint:
- **Transport layer proven** in both MPSL and exclusive modes with real keyboard traffic
- **Multi-pipe PRX** proven on dongle (pipe 0+1) — extends naturally to pipe 2-7 for 3-7 peripherals
- **Runtime mode switch** infrastructure exists — adding a 3rd mode (multi-split) reuses the session-close/open pattern
- **Production validation methodology** established (soak test, latency measurement, power measurement) — reusable for multi-split qualification

Multi-split specific work (post-sprint):
- Address assignment / pairing protocol (currently static binding)
- Channel hopping (if interference testing shows need)
- Dongle keymap merge for >2 peripherals
