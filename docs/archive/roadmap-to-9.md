# Roadmap to 9/10: RMK Integration and Embassy-Quality ESB

Created: 2026-05-20

This document turns the current implementation review into an execution plan.
The target is to raise four project dimensions to 9/10:

- Exclusive ESB core.
- MPSL timeslot coexistence.
- RMK multi-split integration.
- Embassy/open-source contribution readiness.

The first product target remains RMK integration for a tri-mode, multi-split
keyboard. The second target is an upstream-friendly Embassy-style crate.

## Progress Log

### 2026-06-15

- **Phase 1 (baseline/CI):** done. Host checks + example/MPSL/feature-conflict
  checks run in CI. Added an `xtask` runner and on-target register HIL tests
  (`cargo xtask test-hw`, embedded-test on nRF52833 over SWD) — see `tests/hw.rs`.
- **Phase 2 (exclusive ESB core):** hardware-verified (PTX/PRX, ACK payloads,
  multi-pipe, NoAck, suspend/resume).
- **Phase 3 (RMK transport MVP):** the in-repo portion is essentially complete.
  `src/transport.rs` provides `TransportHeader`, `encode_frame`/`decode_frame`,
  `SequenceTracker` (dedup), `StaticBindingTable`, and `accept_bound_frame`,
  with host unit tests. `examples/ptx_split_peripheral` + `prx_split_central`
  demonstrate a split pair carrying an RMK-shaped `SplitMessage` (postcard).
- **Review-fix backlog:** the MPSL PRX PID/CRC cross-slot clearing bug is fixed
  (timeslot state now carries and restores `last_pid`/`last_crc`). Remaining
  backlog items (single global `PTX_BUFS`/`PRX_BUFS`, duplicated timeslot state
  machines) are Phase 5 work.

**Chosen next milestone: Phase 3 → the real RMK adapter (Track A).** The in-repo
transport layer and a working split demo are ready; the missing piece is the
RMK-side `SplitReader`/`SplitWriter` adapter, which lives in the RMK repo
(`/Users/ray/wkspaces/rmk`) because `SplitMessage` is crate-private there.
Immediate validation step: two-nRF52840-dongle bring-up of the split path
before moving into the RMK codebase.

Deferred: Phase 5 (MPSL owned wrapper / pool-based buffers) and Phase 6
(publish split: pure ESB core to crates.io; `mpsl` blocked by the `nrf-mpsl`
git dependency). nRF54L support remains out of scope for now.

## Score Targets

| Area | Current estimate | 9/10 definition |
|------|------------------|-----------------|
| Exclusive ESB core | 7/10 | Stable PTX/PRX async API, multi-pipe ACK payloads, suspend/resume, bounded failure behavior, host tests, and repeatable hardware long-runs. |
| MPSL timeslot coexistence | 5.5/10 | Owned wrapper API, no diagnostic-only global buffers in the public path, stable PRX/PTX timeslot operation, active BLE connection coexistence tuned to keyboard latency/loss targets. |
| RMK integration | 6/10 | RMK split messages run over ESB with static binding, multi-peripheral routing, transport-level sequencing/deduplication, and measured keyboard-style end-to-end behavior. |
| Embassy/open-source readiness | 4/10 | Minimal public API, no PAC leakage in normal use, accurate README, CI matrix, documented safety invariants, MPSL isolated behind optional feature/examples. |

## Key Decisions

| Decision | Choice | Rationale | References |
|----------|--------|-----------|------------|
| Product priority | RMK prototype before finalizing the MPSL abstraction | The real RMK transport will expose the API shape that matters. Avoid designing a polished wrapper around diagnostic assumptions. | RMK split docs state that RMK supports multi-split keyboards and central-to-peripheral communication independently from host transport. |
| RMK payload strategy | Carry RMK's existing serialized split message bytes; do not invent a parallel keyboard protocol first | This minimizes RMK-side blast radius and keeps this crate focused on radio transport. Add only a small ESB transport header for version, device id, sequence number, flags, and length. | RMK split docs describe split central/peripheral communication as a transport abstraction; current local review notes identify `SplitMessage` as the payload contract. |
| ESB compatibility claim | Claim Nordic ESB packet/protocol compatibility, not Gazell wire compatibility | Gazell adds behavior above ESB. The immediate RMK path migrates both sides, so Gazell compatibility is not required. | Nordic ESB documentation describes ESB as a basic bidirectional packet protocol with ACK and retransmission. |
| Pipe handling | Treat pipe as packet metadata via `send_to()` / `send_no_ack_to()` and pipe-filtered ACK queues | Multi-peripheral keyboards cannot rely on mutable ambient `set_pipe()` state when traffic can queue or tasks can interleave. | Current implementation in `src/isr.rs` and `src/payload.rs`; multi-pipe hardware regression in `docs/archive/m10-verification.md`. |
| MPSL slot ending | End using an in-slot TIMER0 compare before the granted slot expires | The application is responsible for tracking slot time and leaving enough cleanup margin before the slot ends. | `nrf-mpsl-sys::mpsl_timeslot_request` docs. |
| MPSL callback design | Keep callback work deterministic and minimal; do not use normal async/locking primitives in the high-priority path | MPSL callbacks and high-priority radio work are timing-sensitive. Current Rust path should use atomics/wakers and tightly controlled mutexes only where proven safe. | Nordic DevZone MPSL guide; `nrf-mpsl` Rust examples and interrupt handlers. |
| BLE coexistence tuning | Treat BLE connection interval, slave latency, ESB slot length, packet density, and priority as first-class parameters | Active BLE connection consumes materially more radio budget than advertising-only. Existing runs show relaxed connection parameters restore useful ESB ACK coverage. | Nordic DevZone guide notes priority and BLE slave latency as scheduling tools; current hardware logs in `docs/archive/m10-verification.md`. |
| Public Embassy style | Consume Embassy peripheral singletons (`Peri<'static, T>`) and use `bind_interrupts!` or explicit ISR hooks | This matches Embassy's ownership and interrupt-binding style and keeps normal users away from PAC details. | Embassy `embassy-nrf` docs for `Peri`, `Peripherals`, and `bind_interrupts!`. |
| MPSL dependency boundary | Keep MPSL and nRF SDC under optional features/examples; do not make them part of the pure ESB core | MPSL and SDC involve Nordic binary libraries and special interrupt/critical-section requirements. Core ESB should remain small and publishable. | `nrf-mpsl` docs; `nrf-softdevice` notes on binary stacks, reserved resources, and critical-section constraints. |

## Phase 1: Baseline, Docs, and CI

Goal: make the current state explicit and create a no-regression gate.

### Work

- Update `README.md`:
  - Replace stale M0 status with the current implementation state.
  - State "Nordic ESB compatible, not Gazell wire compatible".
  - Mark MPSL timeslot examples as experimental/diagnostic until the owned wrapper lands.
  - Document HFCLK ownership in exclusive and MPSL modes.
  - Document supported chip features and reserved status for nRF52833/nRF52832.
- Update `docs/archive/m10-verification.md`:
  - Reconcile the top status table with the later 2026-05-20 hardware results.
  - Split outcomes into "passed", "functional but needs tuning", and "not started".
  - Keep active BLE pipe 1 throughput as an explicit blocker.
- Add CI:
  - Host tests.
  - Exclusive example checks.
  - MPSL example checks.
  - Intentional feature-conflict failure.
  - Formatting.

### Verification

Run locally before opening any follow-up PR:

```bash
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
cargo check --features nrf52840,_cs-cortex
cargo check --features nrf52840,mpsl
cargo check --example ptx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example ptx_multipipe --features nrf52840,defmt,_cs-cortex
cargo check --example ptx_suspend --features nrf52840,defmt,_cs-cortex
cargo check --example mpsl_request_basic --features nrf52840,defmt,mpsl
cargo check --example mpsl_request_chained --features nrf52840,defmt,mpsl
cargo check --example mpsl_ptx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_in_slot --features nrf52840,defmt,mpsl
cargo check --example mpsl_prx_ble --features nrf52840,defmt,mpsl
```

Intentional failure:

```bash
cargo check --features nrf52840,mpsl,_cs-cortex
```

Expected result: compile fails with a clear feature-conflict error.

### Acceptance

- README and verification docs no longer overclaim.
- CI reproduces the local gate.
- New work starts only after this matrix is green.

## Phase 2: Raise Exclusive ESB Core to 9/10

Goal: make non-MPSL ESB independently reliable and reviewable.

### Work

- Add host-side tests:
  - `PacketPool` TX state transitions:
    - `free -> tx_allocated -> tx_queued -> in_dma -> free`.
    - invalid enqueue/claim attempts do not silently publish buffers.
  - `PacketPool::try_dequeue_tx_for_pipe()`:
    - only claims matching pipe.
    - leaves other queued pipes visible for later scans.
  - duplicate detection:
    - first `pid=0, crc=0` packet is not falsely rejected.
    - `valid + pid + crc` is preserved through save/restore.
  - PTX packet metadata:
    - queued packet pipe survives later `set_pipe()` changes.
  - PRX ACK payload filtering:
    - ACK payload queued for pipe 1 is not consumed by pipe 0.
  - config limits:
    - payload length, channel, retransmit count, ACK timeout, CRC length.
- Audit unsafe boundaries:
  - Every `unsafe impl Send/Sync` must have a `SAFETY:` explanation tied to the actual state machine invariant.
  - Every `UnsafeCell` read/write from task context must be protected by IRQ masking or replaced by atomic observable state.
  - Recheck `suspend().await` polling paths for unprotected state reads.
- Tighten API:
  - Keep `send()` / `send_no_ack()` as convenience methods.
  - Document `send_to()` / `send_no_ack_to()` as required for multi-task or multi-pipe traffic.
  - Consider feature-gating or removing `pub use embassy_nrf::pac` from normal public API.
  - Make all library paths return `Result` instead of panicking.

### Hardware Verification

Run on two nRF52840 dongles or equivalent boards:

| Test | Firmware pair | Duration | Pass criteria |
|------|---------------|----------|---------------|
| PTX/PRX basic | `ptx_basic` + `prx_basic` | 30 min | No panic, no stuck radio, counters progress. |
| ACK payload echo | `ptx_ack_echo` + PRX ACK firmware | 30 min | ACK payload count progresses monotonically, no inversion. |
| Multi-pipe exclusive | `ptx_multipipe` + PRX multi-pipe firmware | 30 min | pipe0/pipe1/pipe2 all ACK; no cross-pipe ACK payloads. |
| Suspend/resume | `ptx_suspend` + PRX | 10k cycles or 30 min | No deadlock, restored PID/CRC state, no false duplicate burst after resume. |
| NoAck | NoAck PTX + PRX | 30 min | PRX receives without ACK TX; no disabled shortcut regression. |

### Acceptance

- Host tests cover all PAC-free protocol and buffer invariants.
- Hardware long-runs are recorded in `docs/archive/m9-verification.md` or a new `docs/core-verification.md`.
- Exclusive ESB can be treated as a release candidate for RMK prototyping.

## Phase 3: RMK ESB Transport MVP

Goal: prove the real keyboard transport before finalizing general abstractions.

### Work

- Create an RMK adapter branch or example crate:
  - `RmkEsbWriter`.
  - `RmkEsbReader`.
  - central PRX side routes by pipe/device id.
  - peripheral PTX side sends serialized RMK split messages.
- Transport frame:

```text
byte 0: protocol_version
byte 1: device_id
byte 2: sequence_number
byte 3: flags
byte 4: payload_len
byte 5..: RMK serialized SplitMessage bytes
```

- Add transport behavior:
  - Static binding table: device id -> pipe/address.
  - `device_id + sequence_number` deduplication.
  - Retry policy above ESB ACK failures.
  - Optional ACK payload for compact reverse control, but do not make correctness depend on a single ACK payload.
- Keep out of MVP:
  - Dynamic pairing.
  - Channel hopping.
  - Encryption.
  - Full GATT configuration UI.

### Verification

| Scenario | Topology | Pass criteria |
|----------|----------|---------------|
| One peripheral | peripheral PTX -> central PRX -> host USB/BLE | Key events arrive once, no duplicate press/release after retransmit. |
| Two peripherals | two PTX devices -> central PRX multi-pipe | Both devices route correctly; no cross-device events. |
| Central BLE host | central PRX + BLE host | Split traffic continues while host remains connected. |
| Dongle mode | dongle exclusive PRX + multiple PTX | Multi-pipe works without BLE scheduler constraints. |
| Power cycle | reboot one peripheral | Static binding resumes without central reset. |
| Loss injection | move peripheral out of range briefly | Transport retries and recovers without stuck keys. |

Metrics to record:

- Application split message success rate.
- Duplicate suppression count.
- ESB max-attempt count.
- p50/p95/p99 split latency.
- BLE connection state.
- Stuck-key count, expected zero.

Target acceptance:

- 30 minute two-peripheral run.
- Application message success rate >= 99.9%.
- p99 split latency within the keyboard target selected for the prototype.
- Zero duplicate key reports after RMK-layer deduplication.

## Phase 4: Active BLE + ESB Timeslot Tuning

Goal: turn current "functional with relaxed parameters" into keyboard-grade coexistence.

### Work

- Build a repeatable tuning harness around:
  - `mpsl_prx_ble`.
  - `mpsl_ptx_in_slot`.
  - RMK transport prototype once Phase 3 exists.
- Expose parameters:
  - BLE connection interval.
  - BLE slave latency.
  - supervision timeout.
  - ESB slot length.
  - in-slot TIMER0 match time.
  - packets per slot.
  - MPSL priority.
  - retry policy for `BLOCKED` / `CANCELLED`.
- Record counters:
  - start, timer0, radio, blocked, cancelled, overstayed.
  - per-pipe tx, ack, ack payload, invalid order.
  - RMK message latency and loss.

### Test Matrix

| BLE mode | ESB mode | Parameters | Pass criteria |
|----------|----------|------------|---------------|
| Advertising only | PRX timeslot multi-pipe | Current 14 ms smoke params | pipe0/pipe1 ACK payloads near baseline. |
| Connected, idle | PRX timeslot multi-pipe | CI 100 ms, latency 4 | No disconnect, pipe0 near baseline, pipe1 usable. |
| Connected, RMK target | PRX timeslot + RMK traffic | CI 7.5/15/30 ms candidates | Meets keyboard latency/loss target. |
| Connected, stress | PRX timeslot + dense packets | high packet density | No panic/assert; degradation is observable and bounded. |
| Collision recovery | Force blocked/cancelled by BLE load | high priority retry enabled | No stuck session, counters reflect retry behavior. |
| Overstay recovery | Intentionally too-tight match margin | diagnostic build only | No panic; overstay counted and session ends safely. |

### Acceptance

- Active BLE connection remains stable for at least 30 minutes.
- No MPSL assert, panic, or unbounded spin.
- No unhandled `OVERSTAYED`.
- RMK split traffic meets the selected keyboard latency/loss target.
- Results are recorded with exact firmware, board, connection parameters, and counters.

## Phase 5: MPSL Owned Wrapper

Goal: replace diagnostic free functions with a reusable Embassy-style API.

### Public API Target

```rust
let ts = EsbTimeslotPrx::wrap(prx, mpsl, cfg)?;
ts.start().await?;
let packet = ts.receive().await?;
ts.send_ack_payload(pipe, payload).await?;
let prx = ts.stop().await?.unwrap();
```

Required types:

- `TimeslotConfig`.
- `ScheduleMode::{Chained, Periodic, Manual}`.
- `TimeslotStats`.
- `EsbTimeslotPtx`.
- `EsbTimeslotPrx`.

### Work

- Move diagnostic APIs behind an `experimental` marker or keep them example-local.
- Replace public-path global static buffers with owned state:
  - wrapper owns session state.
  - caller provides packet pool/buffers.
  - only one active wrapper can own RADIO/TIMER0 at the type/API level.
- Reuse core protocol logic:
  - Prefer reusing `PtxStateMachine` / `PrxStateMachine`.
  - If full reuse is blocked by P0 callback constraints, extract shared helpers for PID, duplicate detection, ACK payload selection, and header construction.
- Make failures observable:
  - no `unwrap()` / `assert!()` in library paths.
  - return `Error` or increment stats for MPSL signals.
- Implement reversible lifecycle:
  - `wrap()` consumes exclusive driver.
  - `stop()` / `unwrap()` closes session and returns the driver.
  - PID, duplicate detection, addresses, and pipe state survive wrap/unwrap.

### Verification

| Test | Pass criteria |
|------|---------------|
| Wrap/unwrap loop | 10k cycles, no resource leak, driver works after unwrap. |
| PTX wrapper | Sends through timeslots with ACK payload continuity. |
| PRX wrapper | Receives through timeslots and queues pipe-specific ACK payloads. |
| Concurrent start attempt | Second wrapper/session returns `Error::Busy`. |
| Block/cancel retry | No stuck session; stats show retry. |
| Active BLE coexistence | Same acceptance as Phase 4. |

### Acceptance

- `src/mpsl_timeslot.rs` no longer exposes diagnostic global-state functions as the primary API.
- MPSL examples use the wrapper.
- Core exclusive examples are unaffected.

## Phase 6: Open-Source and Embassy Contribution Readiness

Goal: make the crate easy to review and easy to discuss upstream.

### Work

- Public API review:
  - No PAC in normal user-facing API.
  - Peripheral ownership through Embassy `Peri<'static, T>`.
  - IRQ story documented: `bind_interrupts!` where possible, manual ISR hooks where necessary.
  - No default feature that pulls Nordic binary libraries.
- Documentation:
  - Quickstart for PTX/PRX.
  - Example matrix.
  - ESB vs Gazell compatibility.
  - Clock ownership.
  - MPSL caveats.
  - Safety invariants.
  - Hardware validation logs.
- Packaging:
  - `Cargo.lock` retained for examples/CI reproducibility.
  - `NOTICE.md` updated for adapted code.
  - MPSL/Nordic binary license caveat clear.
- Community discussion:
  - Open a design issue first, not a large PR.
  - Present the pure ESB core separately from MPSL coexistence.
  - Include measured hardware logs and known limitations.

### Verification

```bash
cargo fmt --check
cargo clippy --lib --target x86_64-unknown-linux-gnu --features nrf52840 -- -D warnings
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
```

Target-specific example checks remain the Phase 1 matrix.

### Acceptance

- A reviewer can understand the core crate without reading MPSL examples.
- MPSL/nRF SDC complexity is optional and isolated.
- README claims match hardware evidence.
- All unsafe code has local, concrete invariants.

## Execution Order

| Order | Phase | Reason |
|-------|-------|--------|
| 1 | Phase 1 | Locks the baseline and prevents accidental regression. |
| 2 | Phase 2 | Gives RMK a stable exclusive ESB base. |
| 3 | Phase 3 | Validates the real keyboard transport before over-designing wrappers. |
| 4 | Phase 4 | Tunes BLE coexistence against keyboard metrics, not synthetic counters only. |
| 5 | Phase 5 | Turns lessons from RMK and tuning into the final MPSL abstraction. |
| 6 | Phase 6 | Packages the result for wider review. |

## Reference Index

Official and project references to use while executing this plan:

- Embassy nRF docs: https://docs.embassy.dev/embassy-nrf/git/nrf52840/index.html
- Embassy `bind_interrupts!`: https://docs.embassy.dev/embassy-nrf/0.3.1/nrf51/macro.bind_interrupts.html
- Nordic MPSL timeslot request docs via `nrf-mpsl-sys`: https://docs.rs/nrf-mpsl-sys/latest/nrf_mpsl_sys/fn.mpsl_timeslot_request.html
- Rust `nrf-mpsl` docs: https://docs.rs/nrf-mpsl/latest/nrf_mpsl/
- Nordic DevZone MPSL guide: https://devzone.nordicsemi.com/guides/nrf-connect-sdk-guides/b/software/posts/updating-to-the-mpsl-timeslot-interface
- RMK split keyboard docs: https://rmk.rs/main/docs/features/split_keyboard
- Nordic ESB user guide: https://developer.nordicsemi.com/nRF51_SDK/nRF51_SDK_v4.x.x/doc/html/group__esb__users__guide.html
- nRF SoftDevice Rust community reference: https://github.com/embassy-rs/nrf-softdevice
- Community MPSL timeslot wrapper reference: https://github.com/inductivekickback/timeslot
- Local current verification log: `docs/archive/m10-verification.md`
- Local lessons learned: `docs/archive/m10-lessons-learned.md`
- Local review backlog: `review-fix-backlog.md`
