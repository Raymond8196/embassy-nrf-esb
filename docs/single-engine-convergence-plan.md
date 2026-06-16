# Single-Engine Convergence Plan (MPSL path → production state machine)

Created: 2026-06-15

This refines **Phase 5 (MPSL Owned Wrapper)** of `docs/roadmap-to-9.md` with a
concrete, staged development + verification plan. Scope is narrow on purpose:
collapse the **two divergent ESB engines** into one, so the MPSL coexistence
path runs the same protocol logic as the exclusive path and can carry real
payloads (the precondition for using it as an RMK split transport).

This is also the first natural **joint task** with the RMK author: it is exactly
where his single-engine (`Esb` + `timeslot_managed`) experience applies.

## 1. Problem (recap)

Today the protocol state machine exists twice:

- **Exclusive path** — `state_machine.rs` (`PtxStateMachine` / `PrxStateMachine`,
  `PacketPool`-backed, RADIO/TIMER ISR-driven) via `isr.rs` (`EsbPtx` / `EsbPrx`).
- **MPSL path** — re-implemented inline inside `mpsl_timeslot.rs`
  (`PtxInnerState` / `PrxInnerState`, fixed `PTX_BUFS` / `PRX_BUFS`, counter-packet
  ACKs, `SIGNAL_RADIO` handlers at ~lines 1024 and 1846). Carries only synthesized
  counter packets, not arbitrary payloads.

Shared already: `EsbRadio` register driver, `EsbHeader`, `config`, `addresses`.
So convergence happens **at the state-machine layer only** — not the register layer.

Cost of the split: protocol bugs/edge cases must be fixed twice and can drift;
the MPSL path can't carry RMK `SplitMessage` bytes; double the test surface.

## 2. Goal / Non-goals

**Goal:** one engine, two modes. The MPSL callbacks call into
`PtxStateMachine` / `PrxStateMachine` (driven from `SIGNAL_RADIO`) instead of
inlining protocol logic. Diagnostics (counters, schedule hints, EXTEND/NORMAL
pacing) stay in the MPSL layer wrapping the engine.

**Target API (from roadmap Phase 5), aligned with haoboGu's `EsbTimeslot::open(&mut esb)`:**

```rust
let ts = EsbTimeslotPrx::wrap(prx, mpsl, cfg)?;   // consumes the exclusive driver
ts.start().await?;
let pkt = ts.receive().await?;                    // real payload, from PacketPool
ts.send_ack_payload(pipe, payload).await?;
let prx = ts.stop().await?;                       // returns the driver back
```

**Non-goals (defer):** nRF54L support, the diagnostic counter/schedule-hint
protocol (moves to a diagnostic wrapper/example, not deleted), publishing to
crates.io, dynamic pairing/channel hopping.

## 3. Four impedance mismatches (why this is not just deletion)

1. **ACK turnaround: auto-shortcut vs manual.**
   Exclusive PRX uses RADIO shortcuts (DISABLED→TXEN) for auto-ACK; MPSL PRX uses
   manual `start_receiving_manual_ack` + `transmit_ack_manual` because the
   shortcut turnaround was unreliable under MPSL callback latency.
   → Add a `timeslot_managed`-style mode to the state machine that selects
   manual vs auto turnaround (mirrors haoboGu forcing `rx_enable`/direct
   `tx_enable` when managed).

2. **Timer model (biggest fork).**
   Exclusive PTX uses `EsbTimer<T>` (TIMER1–4) + PPI for ACK-timeout/retransmit,
   ISR-driven. TIMER0 belongs to MPSL. Today's MPSL PTX uses no ESB timer — it
   relies on in-slot poll (40 µs) + defer-to-next-slot.
   → **Decision required** (see §7-D1): keep in-slot poll/defer, or adopt
   haoboGu's model (ESB timer drives protocol timing even in timeslot mode,
   TIMER0 only schedules slots).

3. **Buffers + payload source.**
   Exclusive uses `PacketPool<N,SIZE>`; MPSL uses fixed single buffers + always
   counter packets. → MPSL mode must drive the same `PacketPool`; counter-packet
   /schedule-hint generation moves out to a diagnostic layer (desirable anyway:
   RMK needs real payloads).

4. **RADIO ISR ownership.**
   Exclusive SM is invoked from the RADIO ISR (NVIC dance in `isr.rs`); MPSL mode
   is invoked from the `SIGNAL_RADIO` callback (MPSL owns the vector; app must NOT
   `NVIC::unmask(RADIO)` — see `docs/esb-ble-coexistence-analysis.md`).
   → The SM core is agnostic; add timeslot entrypoints that skip the NVIC path.

## 4. Target architecture

```
PtxStateMachine / PrxStateMachine          (protocol logic — single copy)
  + mode flag `timeslot_managed`:
      auto shortcut turnaround   ↔  manual turnaround
      PPI/EGU kick-off           ↔  direct tx_enable/rx_enable
      RADIO-ISR entrypoint       ↔  timeslot SIGNAL_RADIO entrypoint
  + PacketPool for buffers in both modes

MPSL layer (mpsl_timeslot.rs) keeps ONLY:
  session lifecycle, EARLIEST/NORMAL scheduling, EXTEND, SignalCounters,
  BLOCKED/CANCELLED/SESSION_IDLE/OVERSTAYED recovery, RADIO handoff
  (mpsl_radio.rs). Borrows the driver (wrap/unwrap), calls sm methods.

Diagnostics (counter packets, schedule hints, per-pipe stats) move to an
optional diagnostic wrapper / examples — not in the converged engine.
```

## 5. Development plan (staged, PRX-first)

PRX is the hardware-verified strength (100% OK, 99.5% extend yield), so converge
it first to keep the highest-value path low-risk; do PTX second.

- **S0 — Branch + baseline pin.** Branch from current `feat/mpsl-timeslot`. Tag the
  current inline-engine MPSL build so the verified result can be re-run for
  regression comparison.
- **S1 — Decide PTX timing model (D1).** Blocks S5. Optionally a short spike.
- **S2 — State-machine mode flag.** Add `timeslot_managed` (or a `RadioDrive`
  enum) to `PtxStateMachine` / `PrxStateMachine`: manual vs auto ACK turnaround,
  direct enable vs PPI, and timeslot entrypoints (`start_rx_in_slot`,
  `handle_radio_event` callable without the NVIC dance). ~+150–250 lines; protocol
  body unchanged.
- **S3 — Ownership/wrap.** `isr.rs`: let the MPSL session borrow `&mut EsbPrx` /
  `&mut EsbPtx` (analogous to `EsbTimeslot::open(&mut esb)`); expose the timeslot
  entrypoints. Decide wrap-by-value vs by-mut-ref (see §7-D2).
- **S4 — Rewire PRX MPSL callback.** Replace the inline PRX `SIGNAL_RADIO` body
  (~lines 1846–2003) with calls into `PrxStateMachine`. Remove PRX protocol fields
  from `PrxInnerState` (`last_pid`/`last_crc`/`last_valid`). Keep counters,
  scheduling, EXTEND. ACK payloads come from `PacketPool` via `send_ack_payload`.
- **S5 — Hardware re-verify PRX** (see §6). Must match the verified baseline.
- **S6 — Rewire PTX MPSL callback** per D1. Remove `PtxInnerState` protocol logic.
- **S7 — Hardware re-verify PTX.**
- **S8 — Diagnostics relocation.** Move counter-packet + schedule-hint generation
  (`write_counter_packet`, `write_ack_counter_packet`, `mpsl_schedule`) into a
  diagnostic wrapper / examples so the core engine carries real payloads.
- **S9 — Cleanup.** Delete now-dead inline state; `mpsl_timeslot.rs` net −800…−1200
  lines; update `docs/roadmap-to-9.md` Phase 5 status.

## 6. Test & verification plan

Three layers; nothing merges to the integration branch until all three are green
for the converged role.

### 6.1 Host unit tests (`cargo xtask test-host`)
- Reuse/extend existing module tests (`state_machine` invariants via `PacketPool`,
  dup detection save/restore, ACK pipe filtering, header/config limits).
- Add: state machine behaves identically in `timeslot_managed = true/false` for
  the pure-logic transitions (mode flag only changes radio kick-off, not protocol
  decisions). Pure-logic assertions only (no PAC).

### 6.2 On-target register HIL (`cargo xtask test-hw`, nRF52833 over SWD)
- Existing `tests/hw.rs` asserts `EsbPrx` programs RADIO/TIMER1 correctly.
- Add a HIL case asserting the **timeslot-managed** init path programs the same
  RADIO packet/CRC/address config and the manual-ACK shortcut set, so the mode
  flag doesn't silently change register state.

### 6.3 Exclusive-path regression (2× nRF52840 dongles, DFU)
- Run the Phase 2 matrix (PTX/PRX basic, ACK echo, multi-pipe, suspend/resume,
  NoAck) to prove the mode flag didn't regress the exclusive engine.

### 6.4 MPSL coexistence re-verification (the gate that matters)
Reuse `scripts/ble_conn_capture.py` and the existing examples
(`mpsl_3mode_central` / `mpsl_3mode_event`, `mpsl_prx_ble`, `mpsl_ptx_in_slot`).

| Run | Firmware | Config | Pass criteria (match verified baseline) |
|-----|----------|--------|------------------------------------------|
| PRX baseline replay | pre-convergence tag | NordicExtend, BLE connected, 120 s | reproduce ~100% OK / 99.5% extend yield (anchor) |
| PRX converged | converged build | same | OK rate within noise of baseline; no rx=0 |
| 3-mode converged | converged | ESB+BLE HID+USB CDC, 120 s | zero ESB degradation vs baseline |
| PTX converged | converged | per D1 | first-attempt success ≥ baseline; bounded retries |
| Recovery | converged | force BLOCKED/CANCELLED/OVERSTAYED | no stuck session; counters reflect recovery |

Metrics recorded each run (capture script already emits these): OK rate, acked/s,
p50/p99 latency, TX first-attempt success, extend yield, BLOCKED/CANCELLED, plus
ESB-during-BLE OK-rate delta.

**Acceptance:** converged PRX/PTX meets or beats the verified baseline; exclusive
regression matrix green; host + HIL green.

## 7. Decisions

- **D1 — PTX timing model. DECIDED: B2 (reuse protocol logic, mode-split timing).**
  Converge the *protocol logic* (PID, duplicate detection, ACK / repeated-ACK,
  attempts) onto the shared `PtxStateMachine`/`PrxStateMachine`; abstract the
  *timing source* by mode — exclusive uses `EsbTimer<T>` + PPI (automated),
  timeslot keeps the already-verified **TIMER0 CC1 / in-slot poll** ACK-timeout.
  - Rejected B1 (full haoboGu-style EsbTimer+PPI in timeslot): the production SM's
    ACK-timeout/retransmit is driven by `TIMER-ISR → pend RADIO ISR → RADIO ISR`,
    but in timeslot mode the RADIO ISR belongs to MPSL — that chain is broken, so
    B1 would require rewriting the SM timing to be fully PPI-automated + the
    highest PTX re-verification risk.
  - Rejected A (keep a separate PTX timeslot path): minimal reuse, not real
    convergence.
  - This matches roadmap Phase 5's stated fallback ("extract shared helpers for
    PID, duplicate detection, ACK payload selection, header construction").
  - **Note:** today's MPSL PTX (mpsl_timeslot.rs ~1024–1135) already uses TIMER0
    CC1 manual ACK-timeout with no PPI retransmit — B2 preserves that timing, so
    PTX behavior changes least. **D1 only gates S6–S7 (PTX); PRX S1–S5 is
    independent.** Do a short PTX micro-spike at the S6 boundary to confirm the
    TIMER0/poll timing feeds the shared SM with unchanged behavior.
  - PRX is "pure B2": no ACK-timeout timer (RX→ACK→RX driven by SIGNAL_RADIO), so
    almost nothing to abstract on the timing side.
- **D2 — Wrap ownership.** Recommendation: `wrap()` consumes `EsbPrx`/`EsbPtx` by
  value and `stop()` returns it (clean lifecycle, matches roadmap Phase 5 API).
  Confirm at S3.
- **D3 — Sequencing vs roadmap.** Convergence elevated ahead of / parallel to the
  roadmap's Phase 3 RMK-adapter step, because it's the precondition for the MPSL
  path to carry real RMK payloads.
- **D4 — Collaboration timing. DECIDED: solo through the full convergence**, then
  bring in the RMK author. (No mid-way review gate; S1–S9 run solo.)
- **D5 — Hardware. DECIDED: fully equipped; the author runs all hardware/HIL and
  coexistence verification** (2× nRF52840 dongles + BLE central + capture host;
  Elytra nRF52833 + SWD probe). No need to defer hardware-dependent stages.

## 8. Risks & rollback

- **Primary risk:** the 100% OK coexistence result was achieved with the *inline*
  engine. Convergence can regress it until re-verified. Mitigation: PRX-first,
  baseline replay anchor (S0 tag), hardware gate at S5/S7 before proceeding.
- **Secondary:** manual/auto ACK turnaround behaves differently under MPSL latency
  — covered by 6.2 + 6.4.
- **Rollback:** keep the inline-engine tag; if a converged role can't match
  baseline within an agreed window, revert that role and keep the inline path
  while investigating.

## 9. S3 + S4 implementation spec (PRX)

S3 and S4 are one unit: the wrapper is inert without the callback driving it, and
the callback can't drive a state machine it doesn't hold. Spec'd together so the
code pass is mechanical and reviewable. (S2 foundation — the `timeslot_managed`
flag + accessors — is already landed on both state machines.)

### D6 — generic SM ↔ non-generic global callback bridge. DECIDED: trait object.

`EsbPrx<T, N, SIZE>` is generic; the global `PRX_STATE` / `prx_timeslot_callback`
cannot name it for arbitrary `T`. Bridge with an object-safe trait:

```rust
// isr.rs (or a small mpsl bridge module)
pub(crate) trait TimeslotPrxDriver: Sync {
    fn ts_start_rx(&self);   // slot start: timeslot_managed=true, manual-ACK RX arm
    fn ts_on_radio(&self);   // SIGNAL_RADIO: drive one PrxStateMachine event
    fn ts_force_stop(&self); // slot end / EXTEND_FAILED / handoff
}
impl<T: TimerInstance, const N: usize, const SIZE: usize> TimeslotPrxDriver
    for EsbPrx<T, N, SIZE> { /* mask-free SM calls; no NVIC dance */ }
```

`PRX_STATE` stores `Option<&'static dyn TimeslotPrxDriver>` (set at `wrap()`).
Rationale: `EsbPrx` already lives in a `&'static` (StaticCell) and is `Sync`; the
trait object erases `T/N/SIZE` so the global stays non-generic; one vtable call
in the P0 callback is negligible (no locking/async added — satisfies the roadmap
"deterministic, minimal P0 callback" constraint). Reversible if it ever complicates
timing.

### S4-a — PrxStateMachine manual-ACK branches (gated on `timeslot_managed`)

The `timeslot_managed == false` path is the existing verified exclusive behavior —
untouched. The `true` path mirrors today's verified inline PRX (mpsl_timeslot.rs
~1846–2003), reusing the mpsl-gated radio methods:

| Point in `handle_radio_event` | exclusive (false) | timeslot (true) |
|---|---|---|
| `start_receiving` | `radio.start_receiving` (auto `disabled_txen`) | `radio.start_receiving_manual_ack` |
| Receiver → new + ack | `setup_ack_tx` (ramps via shortcut) → `TxAck` | read `rx_match`, dequeue ACK buf, `radio.transmit_ack_manual(pipe, ack_dma)` → `TxAck` |
| Receiver → dup + ack | `setup_ack_tx_fallback` → `TxRepeatedAck` | re-send active ACK / fallback via `transmit_ack_manual` → `TxRepeatedAck` |
| Receiver → new/dup + noack | `complete_rx_no_ack` | `radio.stop()` + `start_receiving_manual_ack` |
| TxAck / TxRepeatedAck disabled | `complete_rx_ack` | `start_receiving_manual_ack` (fresh RX buf) |

Encapsulate the branch in 3 private helpers (`arm_rx`, `start_ack_tx`,
`after_ack_restart`) so `handle_radio_event` stays readable and the diff is local.
PRX has no ACK-timeout timer in either mode, so no timing abstraction needed here
(the D1/B2 timing work is PTX-only, S6).

### S4-b — callback rewire (mpsl_timeslot.rs)

- `prx_timeslot_callback`: replace the inline protocol body with
  `SIGNAL_START → driver.ts_start_rx()`, `SIGNAL_RADIO → driver.ts_on_radio()`,
  slot-end/EXTEND_FAILED/etc `→ driver.ts_force_stop()`.
- **Keep unchanged:** `SignalCounters`, EARLIEST/NORMAL scheduling, EXTEND logic +
  slot-end TIMER0, BLOCKED/CANCELLED/SESSION_IDLE/OVERSTAYED recovery,
  `mpsl_radio` handoff.
- **Remove from `PrxInnerState`:** `last_pid`/`last_crc`/`last_valid` (now owned by
  `EsbRadio` inside the SM); inline counter-packet ACK generation.

### S4-c — ACK payload source

Counter packets + schedule hints move OUT of the engine to the diagnostic
layer/example: the app queues ACK payloads via `prx.send_ack_payload(pipe, bytes)`;
the engine transmits whatever is queued (empty-ACK fallback if none) — identical to
exclusive PRX. This is what lets the MPSL path carry real RMK payloads.

### S4-d — wrap/unwrap lifecycle (S3 surface)

```rust
let ts = EsbTimeslotPrx::wrap(prx, mpsl, slot_cfg)?; // sets timeslot_managed, registers &dyn, opens session
ts.start().await?;                                   // first EARLIEST request
let pkt = ts.receive().await?;                       // delegates to prx.receive()
ts.send_ack_payload(pipe, payload).await?;           // delegates to prx
let prx = ts.stop().await?;                          // close session, timeslot_managed=false, clear PRX_STATE, force_stop
```

### Verification (per §6)
Host/HIL: build all feature sets + register HIL still green. Hardware (S5): the
converged `mpsl_3mode_central`/`mpsl_prx_ble` against `ble_conn_capture.py` must
reproduce the baseline (≈100% OK, 99.5% extend yield) vs the `baseline-inline-engine-mpsl`
tag before PTX (S6) starts.

## 10. Progress

- **2026-06-15** — S0/S2/S4-a landed on `feat/single-engine-convergence`
  (baseline tag `baseline-inline-engine-mpsl`):
  - S2: `timeslot_managed` mode + accessors on both state machines (commit 3e766ba).
  - S4-a: PRX manual-ACK turnaround via mode-aware helpers; exclusive codegen
    unchanged under `not(feature="mpsl")`; ACK payloads from `PacketPool` (commit 6bc9e8d).
  - Verified without hardware: exclusive + mpsl `cargo check` clean, host tests 52/0.
  - **Remaining is hardware-coupled** (deferred to a bench session): S3 wrapper +
    S4-b callback rewire are coupled (the wrapper is inert until the callback
    drives it) and only S5 can confirm the converged PRX reproduces the baseline;
    S6/S7 (PTX) need the D1 micro-spike on hardware.

- **2026-06-16** — S3 + S4-b landed (uncommitted, on branch):
  - **S3 (D6 trait-object bridge):** `TimeslotPrxDriver` trait in `isr.rs` with
    `ts_start_rx` / `ts_on_radio` / `ts_force_stop` / `ts_snapshot_dup_state` /
    `ts_last_pipe`. `EsbPrx::new_timeslot()` constructor (no NVIC RADIO unmask —
    MPSL owns the vector). `PrxStateMachine` gained `addresses` field +
    `ts_start_rx/ts_on_radio/ts_force_stop` methods (power-cycle + init + restore
    dup-state + arm RX each slot).
  - **S4-b (callback rewire):** `prx_timeslot_callback` SIGNAL_START / SIGNAL_RADIO
    / slot-end now dispatch through `driver.ts_*()` when `state.driver.is_some()`.
    Legacy inline path preserved as fallback (`driver == None`). Per-pipe counters
    updated from `PrxEvent` + `ts_last_pipe()`. `set_prx_driver()` / `clear_prx_driver()`
    public API for session-time registration.
  - **mpsl_3mode_central.rs** updated: creates `EsbPrx<TIMER1>` via
    `new_timeslot`, calls `set_prx_driver`. USB CDC setup moved before BLE init
    so logs are visible even if BLE panics.
  - **Verified:** `make check-no-hw` green (host 52/0, exclusive, MPSL, fmt,
    feature-conflict). On-target (1× nRF52840 dongle): converged PRX firmware
    boots, USB CDC active, `[3MODE] PRX converged engine active` logged,
    timeslot scheduling active (start/timer0 counters incrementing, blocked
    recovery working). `ts_start_rx` returns Ok (no buffer alloc failures).
  - **Pending S5:** full PTX↔PRX coexistence verification blocked on second
    dongle recovery (DC08665938A2 stuck after repeated BLE-init flashes).
    `rd=0` in solo-run is expected (no PTX transmitting → no RADIO events
    within slot; slot-end stop doesn't generate SIGNAL_RADIO).

- **2026-06-16 follow-up** — review fixes + single-dongle smoke landed:
  - **Headless diagnostic RX drain:** converged timeslot PRX now discards packets
    queued by `PrxStateMachine` after the callback has updated diagnostic counters.
    This prevents `PacketPool<4, 256>` exhaustion in examples that do not have an
    app task draining `prx.receive()`.
  - **Diagnostic ACK fallback parity:** when no app ACK payload is queued, the
    converged manual-ACK path now emits the same counter + schedule-hint ACK payload
    that the legacy inline PRX engine generated. This preserves S5 PTX-side
    observability (`ack_payload_count`, `last_ack_counter`, schedule hints) while
    still letting real app ACK payloads come from `PacketPool`.
  - **Driver lifecycle cleanup:** `PrxInnerState::reset_runtime()` and
    `PrxSlotSession::drop()` clear `state.driver`, so a registered
    `TimeslotPrxDriver` cannot leak across PRX sessions.
  - **Verified:** `make check-no-hw` green (host 52/0, exclusive, MPSL, fmt,
    feature-conflict). Rebuilt `mpsl_3mode_central` release firmware, generated a
    fresh DFU package in `/tmp/mpsl_3mode_central_codex.zip`, and flashed one
    nRF52840 dongle via `nrfutil dfu usb-serial`.
  - **Single-dongle smoke result:** device re-enumerated as USB CDC `Central`;
    logs showed `[3MODE] PRX converged engine active`; `s`/`t0` continued to
    advance; solo run had expected `rx=0 rd=0`; error counters stayed quiet
    (`iv=0`, `ov=0`, `dt=0`, `sc=0` in sampled reports).
  - **Still blocked on 2× dongles:** full S5 replay still needs an active PTX peer
    to prove RX throughput, duplicate detection under retransmit, ACK payload
    counters, and schedule-hint behavior against the baseline.

### Single-dongle work still available before S5

- Add host-side/unit coverage for the new diagnostic ACK fallback and RX-drain
  behavior by factoring the pure payload/counter pieces into testable helpers.
- Tighten `TimeslotPrxDriver` API shape before it becomes public surface: decide
  whether diagnostic-only methods should remain in the trait or move behind a
  session wrapper/internal adapter.
- Implement the S4-d `EsbTimeslotPrx` wrapper skeleton around
  `open_prx_session`/`set_prx_driver`/`clear_prx_driver`, with compile-only tests
  and docs. Single-dongle smoke can verify open/start/drop lifecycle and driver
  cleanup, but not RX/ACK correctness.
- Add a single-dongle regression example or script that flashes
  `mpsl_3mode_central`, captures CDC logs for a fixed window, and asserts the
  smoke invariants: CDC up, converged active, `s/t0 > 0`, `iv/ov/dt/sc == 0`,
  and `rx=rd=0` accepted in no-peer mode.
- Do PTX S6 design-only preparation: factor the reusable timer-mode interface and
  keep it compile-checked. Hardware acceptance for PTX still waits for S7 with a
  peer dongle.

- **2026-06-16 single-dongle prep follow-up:**
  - Added host tests for the review-fix invariants: `PacketPool::discard_received`
    drains queued RX buffers back to free, and `write_timeslot_diag_ack` preserves
    the legacy counter + schedule-hint ACK payload layout.
  - Tightened driver/session configuration: `TimeslotPrxDriver::ts_configure`
    is now part of the trait, `set_prx_driver()` calls it, and
    `mpsl_3mode_central` passes `prx_cfg.enabled_pipes` so the shared state
    machine listens on the same pipe mask as the PRX session.
  - Added the S4-d lifecycle skeleton `EsbTimeslotPrx<T,N,SIZE>` around
    `open_prx_session` + `set_prx_driver`, with `receive`, `send_ack_payload`,
    `next_report`, and `stop` forwarding methods. This is compile-checked only
    for now; examples still use the explicit diagnostic calls.
  - Added `scripts/smoke_single_dongle_3mode.py` as the first automation pass for
    build/package/DFU/log assertion. The script can build/package and contains
    the log invariant checks, but the local dongle's USB CDC/DFU node currently
    disappears during scripted open/capture; manual `nrfutil dfu usb-serial -snr`
    plus `timeout cat /dev/ttyACM0` remains the reliable verification path.
  - **Verified:** `make check-no-hw` green with host tests now 54/0. Manually
    flashed the latest build by serial number (`C2A1EFA145C4`), device
    re-enumerated as `Central`, and sampled reports again showed converged PRX
    active with `s/t0 > 0`, expected no-peer `rx=0 rd=0`, and quiet error
    counters (`iv=0`, `ov=0`, `dt=0`, `sc=0`).
