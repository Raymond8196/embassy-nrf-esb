# nRF-ESB: Pure Rust ESB + MPSL Timeslot Implementation Plan

## Context

RMK keyboard project currently uses Nordic Gazell (proprietary, precompiled C library) for 2.4GHz wireless. Gazell blocks upstream merge (license issue), can't coexist with BLE via MPSL, and is single-role locked. The goal is to replace it with a pure Rust ESB implementation that:

1. **Phase 5A**: Works in exclusive RADIO mode (drop-in Gazell replacement)
2. **Phase 7**: Integrates with MPSL timeslots for BLE+ESB concurrent operation
3. Is an independent, open-sourceable crate with Embassy async API

The reference implementation is `esb-ng` (jamesmunns/esb, forked to Raymond8196/esb, cloned to `/home/qlg/wkspaces/esb-ng/`) which provides a working ESB state machine but is architecturally incompatible (bbq2, nrf-pac 0.1, NVIC::pend, no MPSL awareness).

## References

### Code References

| Resource | Location | Relevance |
|----------|----------|-----------|
| **esb-ng** (jamesmunns/esb) | `/home/qlg/wkspaces/esb-ng/` | Primary reference for RADIO register ops, PTX/PRX state machines, timing constants |
| **NCS esb_ptx_ble** sample | `github.com/nrfconnect/sdk-nrf/samples/esb/esb_ptx_ble/` | Nordic's official ESB+BLE concurrent sample; `CONFIG_ESB_MPSL_TIMESLOT=y` architecture |
| **NCS esb_prx_ble** sample | `github.com/nrfconnect/sdk-nrf/samples/esb/esb_prx_ble/` | PRX counterpart of above |
| **too1/ncs-esb-ble-mpsl-demo** | `github.com/too1/ncs-esb-ble-mpsl-demo/` | Low-level MPSL timeslot handler with manual ESB suspend/resume |
| **inductivekickback/ncs_ble_esb_demo** | `github.com/inductivekickback/ncs_ble_esb_demo/` | Radio Notification approach; PID persistence pattern; ZLI workaround |
| **Nordic DevZone MPSL Guide** | `devzone.nordicsemi.com/.../updating-to-the-mpsl-timeslot-interface` | Conceptual framework for MPSL timeslots |
| **RMK Phase 4.1 PoC** | `examples/use_rust/nrf52840_radio_switch_poc/` | Proven dynamic RADIO ISR dispatch (AtomicU8 pattern) |

### Official Documentation

| Topic | Source | URL |
|-------|--------|-----|
| **nRF52840 Product Specification** | Nordic PS v1.7 | https://docs.nordicsemi.com/bundle/ps_nrf52840/page/keydoc_html.html |
| **RADIO Peripheral** (PS Ch 6.17) | Nordic PS | https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html |
| **TIMER Peripheral** (PS Ch 6.24) | Nordic PS | https://docs.nordicsemi.com/bundle/ps_nrf52840/page/timer.html |
| **CRCCNF Register** — SKIPADDR field | Nordic PS §6.17.10 | CRC excludes address when SKIPADDR=1 (ESB standard; avoids errata [143]) |
| **PCNF0 Register** — S1INCL, LFLEN, S1LEN | Nordic PS §6.17.10 | S1INCL=Auto (reset default): S1 included in RAM when S1LEN>0 |
| **PCNF1 Register** — MAXLEN, BALEN, ENDIAN | Nordic PS §6.17.10 | ENDIAN=Big for ESB; MAXLEN=252; BALEN=4 (5-byte address) |
| **EasyDMA / PACKETPTR** | Nordic PS §6.17.6 | DMA pointer must be word-aligned; cannot access RAM block 1 (see errata [122]) |
| **Errata [122]** EasyDMA RAM block 1 | Nordic Errata | https://docs.nordicsemi.com/bundle/errata_nrf52840_Rev3/page/ERR/nRF52840/Rev3/latest/err_840.html |
| **Errata [153]** RSSI inaccuracy | Nordic Errata | https://docs.nordicsemi.com/bundle/errata_nrf52840_Rev3/page/ERR/nRF52840/Rev3/latest/anomaly_840_153.html |
| **Errata [204]** TX/RX emissions | Nordic Errata | https://docs.nordicsemi.com/bundle/errata_nrf52840_Rev1/page/ERR/nRF52840/Rev1/latest/anomaly_840_204.html |
| **Nordic ESB User Guide** (nRF5 SDK) | Nordic SDK Docs | https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html |
| **S1INCL Behavior** | Nordic DevZone | https://devzone.nordicsemi.com/f/nordic-q-a/79717 |
| **MPSL Timeslot API** | nRF Connect SDK Docs | https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/mpsl/doc/timeslot.html |

## Expert Review Findings

> Three independent reviews: Rust expert, RMK integration expert, ESB/BLE protocol expert.

### Critical Issues (must fix before implementation)

**R1. `EsbTimer::regs()` — no singleton enforcement (Rust expert)**
nrf-pac 0.3 exposes `pub const TIMER1: Timer` — anyone can access the hardware directly. The trait's `regs()` provides zero compile-time protection.
**Fix**: Accept `embassy_nrf::peripherals::TIMER1` (singleton) in constructor, convert to PAC internally, expose only via `pub(crate)` on the `EsbIrq` handle.

**R2. `PacketPool` DMA use-after-free — in-flight state missing (Rust expert)**
Three-channel design (`free`, `tx_queue`, `rx_queue`) with `usize` indices has no "in-flight" state. When a packet's pointer is given to RADIO DMA, the index is not in any channel.
**Fix**: Adopt esb-ng's grant-passing pattern — hold `Option<PayloadR>`/`Option<PayloadW>` in the `EsbRadio` struct during DMA. Or add explicit `AtomicU8` state per slot (free/queued/in_dma/rx_done).

**R3. `MaybeUninit` wrong for DMA buffers — no alignment guarantee (Rust expert)**
`MaybeUninit<[u8; SIZE]>` aligns to 1 byte. RADIO DMA requires word-aligned PACKETPTR ([PS §6.17.6](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html)). Also lacks interior mutability.
**Fix**: Use `UnsafeCell<[u8; SIZE]>` with `#[repr(C, align(4))]`.

**R4. Feature gates allow broken compile states (Rust expert)**
`default = ["timer1"]` means `default-features = false` = unhelpful compile error. Multiple timer features = conflicting impls.
**Fix**: Remove default, add `compile_error!` guards for mutual exclusion and at-least-one.

**R5. `CRCCNF.SKIPADDR` not explicitly configured (Protocol expert)**
esb-ng relies on reset default of `1` (skip address in CRC). If this changes, ESB CRC silently fails. Related to [errata [143]](https://docs.nordicsemi.com/bundle/errata_nrf52840_EngA/page/ERR/nRF52840/EngineeringA/latest/anomaly_840_143.html).
**Fix**: Explicitly set `CRCCNF.SKIPADDR = 1` in radio init.

**R6. Off-by-one in retransmit attempt count (Protocol expert)**
esb-ng uses `attempts > max` (strictly greater), giving 4 attempts instead of configured 3. Likely a bug.
**Fix**: Use `>=` in the new implementation. Verify against Nordic C ESB reference (`nrf_esb.c`).

**R7. Errata [122] — DMA buffers must be in RAM block 0 (Protocol expert)**
EasyDMA cannot read from RAM block 1 (`0x2003_0000-0x2003_FFFF` on nRF52840). All packet buffers MUST be in RAM block 0.
**Fix**: Ensure `PacketPool` is placed in `.bss`/`.data` (RAM block 0). Use `#[link_section = ".data"]` if needed.

### Important Issues (should fix, may cause subtle bugs)

**R8. Timer CC[0] uses absolute value, CC[1] uses capture+add (Protocol expert)**
esb-ng's `set_interrupt_retransmit()`: CC[0] = absolute + `tasks_clear` + `tasks_start` (from 0). `set_interrupt_ack()`: CC[1] = `tasks_capture` + add relative offset. Plan must document this asymmetry.
**Source**: `esb-ng/src/peripherals.rs` lines 608-616.

**R9. Single-ISR-context architecture not documented (Protocol expert)**
TIMER ISR fires → sets flag → `NVIC::pend(RADIO)` → RADIO ISR handles both timer and radio events. All state machine logic runs in RADIO ISR context. Must not split across two ISRs.
**Source**: `esb-ng/src/irq.rs` lines 45-50.

**R10. NoAck RADIO shortcut not documented (Protocol expert)**
NoAck packets must NOT add `disabled_rxen` shortcut. Radio goes DISABLED after TX END, release buffer immediately.
**Source**: `esb-ng/src/irq.rs` lines 215-219.

**R11. Minimum ACK buffer is 2 bytes `[0,0]`, not 0 (Protocol expert)**
DMA pointer must always point to ≥2 valid bytes: `[length(1), pid_no_ack(1)]`. Zero-length array = out-of-bounds DMA read.
**Source**: `esb-ng/src/peripherals.rs` line 342.

**R12. Builder ownership model has no static anchor (Rust expert)**
`isr_handle` needs `&'static mut` to Radio/Timer, but no `static` container is specified.
**Fix**: Adopt esb-ng's `EsbBuffer` + `try_split(&'static self)` pattern, or use `static_cell::ConstStaticCell`.

**R13. `embassy-sync::Channel` ISR safety (Rust expert)**
Must use `CriticalSectionRawMutex` explicitly. Do not depend on `critical-section` directly — let the user provide the backend (same as RMK does with `nrf-mpsl`).

**R14. ISR-to-async bridging mechanism unspecified (Rust expert)**
Plan doesn't explain how ISR wakes async tasks after replacing bbq2's maitake wakers. `Channel::try_send()` from ISR triggers waker via `CriticalSectionRawMutex`.

**R15. Missing API methods (RMK expert)**
`send_no_ack()`, `stop()`/`disable()`, `is_tx_idle()`, `max_attempts_reached()` callback — all needed for RMK integration. Add to M8 API surface.

**R16. `PCNF0.S1INCL` not explicitly documented (Protocol expert)**
Reset default = Automatic (S1 in RAM when S1LEN>0). Must document dependency; changing S1INCL alters DMA buffer layout.

## Supplementary Review (Independent Rust/ESB Expert — Round 2)

> Review focused on Embassy community contribution and long-term RMK integration goals.

### Architecture Issues

**A1. Timer feature gates don't follow Embassy conventions**
Embassy ecosystem uses generic type parameters for peripheral selection, not feature gates. Current `timer0`/`timer1`/... design causes: only one timer per binary, combinatorial `compile_error!` explosion, style mismatch with Embassy.
**Fix**: Use Embassy standard generics `pub struct Esb<'d, T: TimerInstance>`. User passes concrete timer peripheral, compiler handles constraints. Remove timer feature gates entirely.

**A2. `EsbBuffer::new()` const constraint**
`static ESB_BUF: EsbBuffer<4, 252> = EsbBuffer::new()` requires `EsbBuffer::new()` to be `const fn`, but `embassy-sync::Channel::new()` may not be const in all versions.
**Fix**: Check embassy-sync 0.8 const support. If unavailable, use `static_cell::make_static!` macro (Embassy standard pattern).

**A3. State machine over-decoupling risks timing**
Pure function `ptx_step()` + ISR dispatch layer adds a few hundred ns overhead. ESB ACK window is ~130us — latency must be minimized.
**Fix**: Let state machine hold radio/timer references directly and operate registers inline (matches embassy-nrf driver style). Test with `#[cfg(test)]` mock peripherals. Don't over-decouple.

### Embassy Community Contribution Perspective

**B1. Crate naming inconsistency**
Repo name `embassy-nrf-esb`, crate name `nrf-esb`. Embassy convention: `embassy-nrf-xxx`.
**Fix**: Unify naming. Suggest crate also be `embassy-nrf-esb`, or document naming strategy and upstream path in README.

**B2. Missing generic trait abstraction**
Open-source projects should provide framework-agnostic traits. RMK's `SplitReader`/`SplitWriter` are RMK-specific.
**Fix**: After M8, define `trait EsbTransport` (send/receive). RMK implements externally. Other frameworks can also adapt.

**B3. Missing multi-chip support plan**
Plan only mentions nRF52840. Community will ask about nRF52832/52833 support.
**Fix**: M0 Cargo.toml reserves `nrf52832`/`nrf52833` features (not implemented yet). README documents support roadmap. nRF52832 doesn't have errata [122].

**B4. Missing buildable example templates**
Embassy community evaluates crates by whether examples compile immediately.
**Fix**: M0 includes `.cargo/config.toml` + `memory.x` + `build.rs` template. `cargo build --example ptx_blinky --features nrf52840` must pass.

### ESB Protocol Supplements

**C1. Address configuration API not designed**
ESB address = 4-byte BASE + 1-byte PREFIX. Pipe 0-1 have independent BASE, Pipe 2-7 share Pipe 1's BASE. Needs `EsbAddresses` type in M1.
**Fix**: M1 adds `EsbAddresses` builder with constraint validation.

**C2. Data rate configuration missing**
ESB supports 1Mbps/2Mbps, affecting ramp-up time and timing calculations. Plan mentions `fast-ru` but no data rate selection.
**Fix**: Config adds `data_rate: DataRate` (`OneMbps`/`TwoMbps`), affecting RADIO MODE register and timing constants.

**C3. `suspend()` timing constraints insufficient**
`suspend()` is an async API called from task context, but state machine may be mid-transaction.
**Fix**: `suspend()` implementation must: (1) disable RADIO IRQ to block new transactions; (2) wait/timeout for current transaction completion; (3) save state. MPSL timeslot end callback may not have time to wait — provide `try_suspend()` returning `Err(Busy)`.

**C4. PRX timeslot reliability is an architectural issue**
If dongle (PRX side) also needs BLE, PRX must listen for entire timeslot. Must clarify RMK architecture: dongle is USB-only (exclusive PRX) or also needs BLE.
**Fix**: Clarify in M10. Recommend: dongle runs exclusive-mode PRX (USB-only), keyboard runs PTX + BLE coexistence via MPSL.

### Rust Safety Supplements

**D1. AtomicU8 state transitions need explicit Ordering**
ISR and task context both access slot state — ordering is critical:
- ISR: `in_dma → rx_queued` uses `Release` (DMA data visible to consumer)
- Task: `rx_queued → free` uses `Acquire` (read DMA data before releasing)
- Task: `free → tx_queued` uses `Release` (payload write visible to ISR)
- ISR: `tx_queued → in_dma` uses `Acquire` (read payload written by task)
**Fix**: M4 specifies ordering for each transition. Not all `SeqCst` (wasteful) or all `Relaxed` (unsafe).

**D2. `PacketPool` `Sync` impl needs safety justification**
`UnsafeCell` is not `Sync`, but `static` requires `Sync`. Need `unsafe impl Sync`.
**Fix**: M4 documents safety argument — AtomicU8 ensures only one party (ISR or task) accesses a slot at any time. Code includes `// SAFETY:` comments.

**D3. Cargo.lock should be committed**
Embassy ecosystem crates with examples commit Cargo.lock (CI reproducibility).
**Fix**: Remove `Cargo.lock` from `.gitignore`.

### Timeline Revision

| Milestone | Original | Revised | Reason |
|-----------|----------|---------|--------|
| M2 | 2 days | 3-4 days | nrf-pac 0.3 migration pitfalls |
| M4 | 1-2 days | 2-3 days | Atomic ordering + UnsafeCell safety justification |
| M9 | 3-5 days | 5-10 days | Hardware debugging always takes longer |
| M10 | 5-7 days | 7-14 days | No Rust reference for MPSL integration |
| **Total** | **~35 days** | **~45-55 days** | More realistic estimate |

### Missing Items

| Item | Suggested Milestone | Notes |
|------|-------------------|-------|
| Power management strategy | M8 | Disable RADIO when idle; critical for keyboard battery life |
| Error recovery mechanism | M7 | Watchdog/timeout for RADIO stuck (EVENTS_DISABLED doesn't fire) |
| defmt logging strategy | M7 | Cannot log from ISR, but state transitions need trace-level records |
| CI configuration details | M0 | Minimum: `cargo check` + `cargo test` + `cargo clippy` |
| Version strategy | M0 | embassy-nrf may need git dep (crates.io disallows); affects release plan |

---

## Design Priorities

1. **Self-use in RMK** — Practical, working solution first. API must serve RMK's split keyboard architecture.
2. **Open-source friendly** — Clean API boundaries, Embassy conventions, no PAC types in public API. Easy to refactor into `embassy-nrf-esb` or merge as embassy-nrf module later.
3. **MPSL is hard requirement** — BLE+ESB concurrent operation is a must-have, not optional. Suspend/resume is a core API, not a feature-gated add-on.

## Strategic Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Fork vs. fresh repo | **Fresh repo**, esb-ng as reference | Every module needs fundamental restructuring; clean API > git history |
| Rust edition | **2024** | Matches embassy-nrf 0.10.0; 2024 is now standard in Embassy ecosystem |
| Buffer | **embassy-sync** (not bbq2) | RMK already uses it; bbq2's maitake-sync is a parallel async runtime |
| Timer default | **TIMER1** (generic `<T: EsbTimer>`) | TIMER0 owned by MPSL; TIMER2 used by Gazell; parameterize for flexibility |
| MPSL integration | **Core suspend/resume API + timeslot adapter** | Suspend/resume is part of core design (needed by both MPSL timeslots and BLE/ESB hot-switch). Timeslot adapter is a separate module but not an afterthought |
| MPSL timeslot approach | **Custom handler in Rust** (not Zephyr ESB lib) | Pure Rust ESB; can't use Zephyr's C library. Port the too1/inductivekickback patterns |
| ESB suspend/resume | **Full re-init per timeslot** (not lightweight suspend) | Both reference implementations use full re-init; proven stable; avoids subtle state bugs |
| PID persistence | **Save before suspend, restore after resume** | ESB protocol uses 2-bit PID for duplicate detection; losing PID causes receiver rejections |
| PAC dependency | **via `embassy-nrf::pac` (`unstable-pac` feature)** | nrf-pac 0.3.0 on crates.io; embassy-nrf 0.10.0 re-exports via `unstable-pac` feature. No direct nrf-pac dependency needed |
| ISR binding | **User chooses**: `bind_interrupts!` or manual `#[interrupt]` | Provide `on_radio_interrupt()` method; user can use Embassy macro or RMK's AtomicU8 dispatch |
| Public API surface | **No PAC types exposed** | Wrap all PAC types in newtypes; version upgrades don't break users |

## Crate Structure

```
embassy-nrf-esb/                  (this repo, crate name: nrf-esb)
├── Cargo.toml
├── src/
│   ├── lib.rs                    — Re-exports, Error, Config, constants
│   ├── radio.rs                  — RADIO register access (ref: esb-ng peripherals.rs)
│   ├── timer.rs                  — EsbTimer trait + impl for TIMER1/2/3/4
│   ├── state_machine.rs          — PTX/PRX state machines (ref: esb-ng irq.rs)
│   ├── buffer.rs                 — Static packet pool + embassy-sync channels
│   ├── payload.rs                — EsbHeader, PayloadR, PayloadW
│   ├── suspend.rs                — ESB state save/restore (core API, not feature-gated)
│   ├── async_driver.rs           — EsbPtx, EsbPrx, EsbBuilder (Embassy async API)
│   ├── isr.rs                    — RADIO + TIMER ISR glue
│   └── mpsl_timeslot.rs          — MPSL timeslot adapter (depends on suspend.rs)
├── examples/
│   ├── ptx_blinky.rs             — PTX sends counter, LED toggles on ACK
│   ├── prx_blinky.rs             — PRX receives, LED toggles, sends ACK payload
│   └── ptx_ble_concurrent.rs     — PTX + BLE concurrent via MPSL timeslots
├── .github/workflows/ci.yml
└── README.md
```

**Module dependency graph**:
```
payload ──────────────────────────────────┐
radio ──── state_machine ──── isr ──── async_driver
timer ──── ┘               └── suspend ──┤
buffer ───────────────────────────────────┘
                                         └── mpsl_timeslot
```

`suspend.rs` is a **core module** (not behind a feature gate). It defines:
- `EsbSavedState` — PID, CRC, retransmit count, active pipe, pending TX
- `suspend()` — save state, disable RADIO, stop timer
- `restore()` — full ESB re-init, restore PID, resume from saved state

This serves two consumers:
1. **`mpsl_timeslot.rs`** — save/restore across MPSL timeslots (Phase 7)
2. **RMK BLE/ESB hot-switch** — save/restore when switching radio mode (Phase 4 AtomicU8 dispatch)

## Implementation Discipline

### 1. Every hardware bit decision must cite a source

When writing register values, bit layouts, timing constants, or address formats, every decision must reference one of:
- esb-ng source file and line number (e.g. `esb-ng/src/payload.rs:101`)
- nRF52840 Product Specification section (e.g. "PS §6.17.10 PCNF0")
- Nordic ESB User Guide section

No "I think it should be..." without a citation. When unsure, check esb-ng first.

### 2. Cross-reference esb-ng line-by-line for register and protocol details

The plan is an architectural outline. When implementing specific register writes, bit fields, or timing calculations, open esb-ng side-by-side and verify each value. Do not implement from plan description alone.

### 3. Unsafe boundaries must be enforced by type visibility, not convention

If an internal type has `unsafe impl Send/Sync` or wraps `UnsafeCell`, it must be `pub(crate)` or stricter. Safe public methods must not bypass the safety invariant — if a guard type claims exclusive access, the underlying pool must not also expose a `get()` method.

### 4. Apply compiler_fence symmetrically to TX and RX paths

Every DMA ownership transition needs a fence. If TX release has `compiler_fence(Acquire)`, RX release must too. Review both paths together, not independently.

### 5. Run three-perspective review before declaring a milestone complete

1. **Rust/Embassy**: soundness, API design, Embassy conventions
2. **ESB protocol**: bit layout correctness, timing constants, register values vs esb-ng
3. **RMK integration**: dependency compatibility, ISR coexistence, async model

`cargo check` passing is necessary but not sufficient.

---

## Milestones

### M0: Repository Skeleton (0.5 day)

**Deliverable**: Empty crate compiling for `thumbv7em-none-eabihf`

**Cargo.toml**:
```toml
[package]
name = "embassy-nrf-esb"
version = "0.1.0"
edition = "2024"

[dependencies]
embassy-nrf = { version = "0.10", features = ["unstable-pac"] }
embassy-sync = "0.8"
cortex-m = { version = "0.7", default-features = false }
static_cell = "2"
defmt = { version = "0.3", optional = true }

[features]
# Chip selection (only nrf52840 implemented initially, others reserved)
nrf52840 = ["embassy-nrf/nrf52840"]
nrf52833 = ["embassy-nrf/nrf52833"]
nrf52832 = ["embassy-nrf/nrf52832"]
# Functional options
fast-ru = []
defmt = ["dep:defmt", "embassy-nrf/defmt"]
# NOTE: Timer selected via generics <T: TimerInstance>, no feature gate needed (A1)
# PAC accessed via embassy-nrf::pac (unstable-pac feature, not direct nrf-pac dependency)
# nrf-mpsl will be added in Phase 7 (M10), when MPSL timeslot integration begins
```

**Timer selection** (fixes R4 + A1): No more feature gate mutual exclusion. Embassy standard generic pattern:
```rust
// In user code:
let (ptx, isr) = esb_init::<peripherals::TIMER1>(timer1, radio, config);
// Compiler constrains automatically, no feature conflicts
```

**Chip feature guard** (in `lib.rs`):
```rust
#[cfg(not(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832")))]
compile_error!("One chip feature must be enabled (nrf52840, nrf52833, or nrf52832)");
```

**Example template** (fix B4): M0 deliverable includes `.cargo/config.toml` + `memory.x` + minimal example skeleton. `cargo build --example ptx_blinky --features nrf52840` must pass.

**Version strategy note**: embassy-nrf 0.10.0 and nrf-pac 0.3.0 are both on crates.io. RMK uses the same versions. `unstable-pac` feature is used to access PAC types — semver-unstable but acceptable for an experimental protocol crate. No git dependency needed.

**Open-source design rules**:
- All PAC types accessed via `embassy_nrf::pac::*` — no direct `nrf-pac` import
- Public API wraps PAC in newtypes: `EsbRadio(embassy_nrf::pac::RADIO)`, not exposing raw PAC
- ISR handler is a method: `EsbIsr::on_radio_interrupt()` — user can use `bind_interrupts!` or manual `#[interrupt]`
- `defmt` feature propagates to `embassy-nrf/defmt`

*Fixes R4: no default, compile_error guards. PAC via embassy-nrf re-export.*

**Verification**: `cargo check --target thumbv7em-none-eabihf`

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| nrf-pac 0.3 feature flag names wrong | Low | Low | Check nrf-pac docs for exact feature names before writing Cargo.toml |
| dependency version conflicts with RMK | Low | Low | Both use same crates.io versions (embassy-nrf 0.10.0, nrf-pac 0.3.0); verified compatible |
| unstable-pac semver break | Low | Medium | Acceptable for experimental crate; pin embassy-nrf version if needed |

**Rollback**: Delete the repo and start over. No code at stake.

---

### M1: Core Types — Error, Config, Payload, Header (1-2 days)

**Files**: `src/lib.rs`, `src/payload.rs`

**Reference**: `esb-ng/src/lib.rs` (Error/Config), `esb-ng/src/payload.rs` (EsbHeader)

**Key changes from esb-ng**:
- Remove bbq2 dependency from payload types
- `PayloadR` / `PayloadW` become `&[u8]` / `&mut [u8]` slices with header wrapper
- Keep 4-byte header layout: `[rssi, pipe, length, pid_no_ack]` (hardware-dependent)

**Header DMA layout**:
```rust
/// Do not reorder these fields. The DMA payload offset skips
/// bytes 0-1 (rssi, pipe) — software-only fields. Bytes 2-3
/// (length, pid_no_ack) are the actual RADIO DMA header.
#[repr(C)]
struct EsbHeader {
    rssi: u8,       // [SW] byte 0 — not transmitted
    pipe: u8,       // [SW] byte 1 — not transmitted
    length: u8,     // [HW] byte 2 — RADIO PCNF0.LFLEN
    pid_no_ack: u8, // [HW] byte 3 — RADIO PCNF0.S1LEN
}
const _: () = assert!(EsbHeader::dma_payload_offset() == 2);
```

**EsbAddresses type** (fix C1):
```rust
/// ESB address configuration. Pipe 0-1 have independent BASE, Pipe 2-7 share Pipe 1's BASE.
pub struct EsbAddresses {
    pub base0: [u8; 4],       // 4-byte BASE address for Pipe 0
    pub base1: [u8; 4],       // 4-byte BASE address for Pipe 1-7
    pub prefix: [u8; 8],      // 1-byte PREFIX for each pipe
    pub pipe_count: u8,       // Number of enabled pipes (1-8)
}
```
Builder validates: `pipe_count` 1-8, prefix uniqueness checks.

**DataRate configuration** (fix C2):
```rust
pub enum DataRate {
    OneMbps,   // RADIO MODE = Nrf_1Mbit
    TwoMbps,   // RADIO MODE = Nrf_2Mbit
}
```
Affects: RADIO MODE register, ramp-up time (1M: 130us/40us-fast, 2M: 130us/40us-fast), PCNF0.LFLEN config.

**Config validation** (per [Nordic ESB User Guide](https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html)):
- `ack_timeout >= 44` us
- `retransmit_delay > ack_timeout + 62` us
- `retransmit_delay > RAMP_UP_TIME` (140 us normal, 40 us fast-ru)
- `max_payload <= 252` bytes
- `data_rate` combined with `fast-ru` feature affects timing constants

**Host tests**:
- Header builder round-trip, validation (pipe 0-7, pid 0-3, length 0-252)
- Config validation (boundary tests)
- Config builder chaining

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Header layout mismatch with hardware | Low | Critical | Use exact layout from esb-ng; add compile-time assert for dma_payload_offset=2 |
| Config bounds too restrictive | Low | Low | Follow Nordic ESB defaults; widen if HW testing shows need |
| `PayloadR`/`PayloadW` lifetime complexity | Medium | Medium | Start with owned `[u8; 252]` arrays; add slice wrappers later if needed |

**Rollback**: Revert to esb-ng's payload types and wrap them.

---

### M2: Radio Register Access (2 days)

**File**: `src/radio.rs`

**Reference**: `esb-ng/src/peripherals.rs` lines 60-498

**Official docs**: [RADIO Peripheral PS §6.17](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html), [EasyDMA PS §6.17.6](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html)

**Migration nrf-pac 0.1 → 0.3**:
- API nearly identical (`write(|w| ...)`, `read()`, `modify()`)
- `Radio` is `Copy` in 0.3 (pointer wrapper) — fundamental ownership model change
- Decouple from buffer types: radio layer only tracks DMA pointers

**Struct**:
```rust
pub struct EsbRadio {
    radio: pac::RADIO,
    last_crc: [u16; 8],
    last_pid: [u8; 8],
}
```

Key methods: `init()`, `transmit()`, `prepare_for_ack()`, `check_ack()`, `start_receiving()`, `check_packet()`, `complete_rx_ack()`

**Critical register writes** (must be explicit, not relying on reset defaults):
```rust
// CRCCNF: MUST set SKIPADDR=1 explicitly (R5)
// https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html (CRCCNF register)
radio.crccnf().write(|w| {
    w.set_len(Len::TWO);
    w.set_skipaddr(true);  // CRC excludes address field (ESB standard)
});

// PCNF0: S1INCL=Automatic (reset default), S1LEN=3
// https://docs.nordicsemi.com/bundle/ps_nrf52840/page/radio.html (PCNF0 register)
radio.pcnf0().write(|w| {
    w.set_lflen(len_bits);  // 6 or 8 depending on payload_size
    w.set_s1len(3);         // PID(2) + NO_ACK(1)
    // S1INCL not set: relies on reset default (Automatic)
    // DO NOT change S1INCL without updating DMA buffer layout (R16)
});
```

**compiler_fence placement**:
1. Before `tasks_txen()` / `tasks_rxen()` — `Release` (ensure DMA buffer writes visible)
2. After `crcstatus().read()` — `Acquire` (ensure DMA writes visible to CPU)
3. Around PACKETPTR writes — `Release` before, `Acquire` after

**Event clearing order**: Clear events BEFORE enabling shortcuts/triggering tasks. If EVENTS_DISABLED is already set, shortcut fires immediately.

**Verification**: `cargo check --target thumbv7em-none-eabihf`

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| nrf-pac 0.3 API breakage on specific registers | Medium | High | Side-by-side comparison of esb-ng's 0.1 calls vs 0.3 API; test each register write individually |
| Missing memory barriers around DMA | Medium | Critical | Follow fence placement checklist above |
| RADIO shorts configuration wrong | Low | High | Copy exact shorts from esb-ng: TX mode uses `disabled_rxen`, RX mode uses `disabled_txen`; NoAck mode does NOT add `disabled_rxen` (R10) |
| CRC config mismatch | Low | High | Use ESB standard: CRC=2 bytes, polynomial=0x11021, SKIPADDR=1 (R5) |
| Implicit register defaults change | Low | Critical | Explicitly set CRCCNF.SKIPADDR=1, document PCNF0.S1INCL dependency (R5, R16) |
| DMA buffer in RAM block 1 | Low | Critical | Errata [122]: all PACKETPTR targets must be in RAM block 0 (R7) |

**Rollback**: Wrap esb-ng's peripherals.rs in a compatibility layer if 0.3 migration is blocked.

---

### M3: Timer Abstraction (1 day)

**File**: `src/timer.rs`

**Reference**: `esb-ng/src/peripherals.rs` lines 500-684

**Official docs**: [TIMER Peripheral PS §6.24](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/timer.html)

**Key change**: Remove `PtrTimer::take()` unsafe singleton. Use Embassy standard generic pattern (R1 + A1 fix).

```rust
/// Timer peripheral trait — Embassy generic pattern
pub trait TimerInstance: sealed::Sealed + 'static {
    fn regs() -> pac::timer::Timer;
}

// Implemented for each supported timer (no feature gate, generics handle it)
impl TimerInstance for peripherals::TIMER1 { ... }
impl TimerInstance for peripherals::TIMER2 { ... }
impl TimerInstance for peripherals::TIMER3 { ... }
impl TimerInstance for peripherals::TIMER4 { ... }
// TIMER0 NOT implemented — owned by MPSL

/// Accepts owned peripheral at init time, guaranteeing singleton semantics.
/// ISR accesses via T::regs() (Copy PAC pointer type).
pub struct EsbTimer<T: TimerInstance> {
    _phantom: PhantomData<T>,
}

impl<T: TimerInstance> EsbTimer<T> {
    pub fn new(_timer: T) -> Self {
        // Consumes peripheral ownership — prevents other code from using it
        Self { _phantom: PhantomData }
    }

    /// ISR-safe: T::regs() returns PAC pointer (Copy), no &'static mut needed
    pub(crate) fn regs(&self) -> pac::timer::Timer {
        T::regs()
    }
}
```

**Advantages**: No feature gate mutual exclusion, compile-time timer selection, consistent with Embassy driver style.

**Timer mode**: 32-bit, prescaler=4 (1MHz). NOT 16-bit. ([PS §6.24](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/timer.html))

**CC channel semantics** (R8 — must document):
- **CC[0]** (retransmit): absolute value. `tasks_clear()` + `tasks_start()` — timer counts from 0. Value = `retransmit_delay - RAMP_UP_TIME`.
- **CC[1]** (ACK timeout): relative to current counter. `tasks_capture(1)` reads current count, then CC[1] += `ack_timeout + RAMP_UP_TIME`.

**Verification**: `cargo check --target thumbv7em-none-eabihf`

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Timer CC channel semantics differ from esb-ng | Low | High | CC[0] = absolute (clear+start), CC[1] = relative (capture+add). Document precisely (R8) |
| Static timer access without singleton guard | Medium | Critical | Use `EsbTimerHandle` conversion from embassy peripheral; `regs()` is `pub(crate)` only (R1) |
| 1MHz prescaler calculation wrong | Low | Medium | prescaler=4 (16MHz/16=1MHz), 32-bit mode. Copy exactly from esb-ng |

**Rollback**: Use macro-based timer selection instead of trait if trait design is problematic.

---

### M4: Packet Buffer (1-2 days)

**File**: `src/buffer.rs`

**Replace bbq2 with**: Static packet pool with explicit DMA safety + `embassy_sync::Channel<CriticalSectionRawMutex, ...>`

```rust
#[repr(C, align(4))]  // Word-aligned for RADIO DMA (R3, R7)
pub struct Packet<const SIZE: usize> {
    data: UnsafeCell<[u8; SIZE]>,
}

pub struct PacketPool<const N: usize, const SIZE: usize> {
    storage: [Packet<SIZE>; N],
    state: [AtomicU8; N],  // 0=free, 1=tx_queued, 2=in_dma, 3=rx_queued (R2)
    tx_queue: Channel<CriticalSectionRawMutex, usize, N>,  // ready-to-send
    rx_queue: Channel<CriticalSectionRawMutex, usize, N>,  // received
}
```

**Key design decisions**:
- `UnsafeCell` + `align(4)` instead of `MaybeUninit` — guarantees word alignment and interior mutability (R3)
- `AtomicU8` state per slot — tracks "in_dma" to prevent use-after-free (R2)
- `CriticalSectionRawMutex` explicitly (not `RawMutex` alias) — safe from ISR, no ambiguity (R13)
- All buffers in RAM block 0 (errata [122]) — verified with `#[link_section]` if needed (R7)
- Minimum ACK buffer = 2 bytes `[0,0]` — DMA pointer always points to ≥2 valid bytes (R11)

**Atomic Ordering Specification** (fix D1):
```rust
// State transitions and corresponding ordering:
// Task → ISR direction (task writes data, ISR reads):
//   free → tx_queued:  store(Release)  — payload write visible to ISR
// ISR → Task direction (ISR writes DMA data, task reads):
//   in_dma → rx_queued: store(Release) — DMA received data visible to task
// ISR internal:
//   tx_queued → in_dma: load(Acquire) + store(Relaxed) — read payload
// Task internal:
//   rx_queued → free:   load(Acquire) + store(Relaxed) — read RX data then release
```

**`unsafe impl Sync` safety justification** (fix D2):
```rust
// SAFETY: PacketPool uses AtomicU8 state to guarantee that only one party (ISR or task)
// accesses a given slot's UnsafeCell at any time. State transitions use correct memory
// ordering to ensure cross-context data visibility.
unsafe impl<const N: usize, const SIZE: usize> Sync for PacketPool<N, SIZE> {}
```

**Host tests**: pool alloc/dealloc, queue FIFO ordering, state transitions, alignment assertions

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| DMA pointer invalidation when pool deallocates | High | Critical | `AtomicU8` state: slot transitions to `in_dma` before PACKETPTR write, back to `free` only after ISR confirms RADIO idle (R2) |
| embassy-sync Channel ISR safety | Low | High | `CriticalSectionRawMutex` is ISR-safe (nests `interrupt::free`). Do not use `ThreadModeRawMutex` (R13) |
| Pool exhaustion under load | Medium | Medium | Configurable pool size; `try_send` returns error when full; backpressure to async API |
| DMA buffer in RAM block 1 | Low | Critical | Errata [122]: verify `PacketPool` address range < 0x2003_0000. Use `#[link_section]` if needed (R7) |
| Alignment < 4 bytes | Low | Critical | `#[repr(C, align(4))]` + compile-time `assert!(align_of >= 4)` (R3) |

**Rollback**: Use simple `heapless::spsc::Queue` as fallback if `embassy-sync::Channel` proves problematic in ISR context.

---

### M5: PTX State Machine (2-3 days)

**File**: `src/state_machine.rs`

**Reference**: `esb-ng/src/irq.rs` lines 155-310

**Key architectural change** (revised per A3 — avoid over-decoupling):
State machine holds radio/timer references directly and operates registers inline, rather than returning "actions" for ISR dispatch. This minimizes ISR path latency, matching embassy-nrf driver style. Tests use `#[cfg(test)]` mock peripherals.
```rust
/// State machine operates hardware directly (not pure function) — minimizes ISR path latency
pub struct PtxStateMachine<T: TimerInstance> {
    radio: EsbRadio,
    timer: EsbTimer<T>,
    state: StatePTX,
    // ...
}

impl<T: TimerInstance> PtxStateMachine<T> {
    /// Called from RADIO ISR — reads events, transitions state, writes registers, all internal
    pub(crate) fn handle_radio_event(&mut self, pool: &PacketPool) { ... }
}
```

Host test strategy: Extract pure logic portions as testable functions, but ISR entry operates hardware directly.

**States**: esb-ng has 5 PTX states: `IdleTx`, `TransmitterTx`, `TransmitterTxNoAck`, `TransmitterWaitAck`, `TransmitterWaitRetransmit`.

**Timer calculation asymmetry** (R8):
```
Retransmit delay: config.retransmit_delay - RAMP_UP_TIME  (subtract: radio re-enables)
ACK timeout:      config.ack_timeout + RAMP_UP_TIME       (add: radio ramps to RX)
```

**Retransmit attempt check** (R6): Use `>=` not `>`:
```rust
if self.attempts >= self.config.maximum_transmit_attempts {
    // Drop packet, report MaximumAttempts
}
```

**NoAck path** (R10): When `no_ack=true`, do NOT add `disabled_rxen` shortcut. Radio goes DISABLED after TX END. Release TX buffer immediately, proceed to next packet.

**Single-ISR-context** (R9): TIMER ISR fires → sets timer_flag → `NVIC::pend(RADIO)` → RADIO ISR handles both timer and radio events. All state machine logic runs in RADIO ISR context only.

**Host tests**:
- State transitions with mocked radio/timer events
- Retransmit counter, max attempts exceeded (verify `>=` not `>`)
- ACK received → state reset
- NoAck path (no ACK requested, verify `disabled_rxen` not set)

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Missing state transition in esb-ng → nrf-esb mapping | Medium | High | Enumerate ALL esb-ng PTX transitions (9 total) and verify each has a corresponding `ptx_step` match arm |
| Ramp-up time compensation wrong | Medium | High | Retransmit: `-RAMP_UP`, ACK: `+RAMP_UP`. Copy esb-ng's exact calculations (R8) |
| ACK timeout window too narrow | Medium | High | Default: wait_for_ack_timeout=120us + RAMP_UP_TIME=140us = 260us total. Verify on HW at 2Mbps |
| Retransmit off-by-one | Low | Medium | Use `>=` for attempt check, not `>` (R6) |

**Rollback**: Copy esb-ng's `irq.rs` PTX handler verbatim and wrap it if pure-function extraction proves too complex.

---

### M6: PRX State Machine (2-3 days)

**File**: `src/state_machine.rs` (same file, `prx_step` function)

**Reference**: `esb-ng/src/irq.rs` lines 312-425

esb-ng has 4 PRX states: `IdleRx`, `Receiver`, `TransmittingAck`, `TransmittingRepeatedAck`.

**Duplicate detection**: Use exact CRC+PID check from esb-ng: `(last_crc[pipe] == crc) && (last_pid[pipe] == pid)`. 8 pipe entries.

**Minimum ACK buffer** (R11): Fallback ACK = `[0u8; 2]` (2 bytes: length + pid_no_ack).

**Host tests**:
- CRC failure → auto-restart
- Repeated packet detection (same CRC + PID)
- Empty ACK fallback (no TX queued)
- NoAck packet handling

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Duplicate detection false negatives | Medium | High | Use exact CRC+PID check. Updated on each valid RX |
| ACK TX timing violation | Medium | High | RX mode `disabled_txen=false`. TX must start within ~130us of RX complete |
| PRX doesn't return to RX after ACK TX | Medium | High | State machine must always transition back to `Receiver`; mock tests verify |
| NoAck packet not passed to application | Low | Medium | NoAck packets delivered to rx_queue but NOT trigger ACK TX |

**Rollback**: Same as M5 — copy esb-ng's PRX handler if extraction fails.

---

### M7: ISR Glue (1 day)

**File**: `src/isr.rs`

User provides ISR wrapper in their application:
```rust
// In user's app:
#[embassy_nrf::pac::interrupt]
fn RADIO() {
    ESB_ISR.on_radio_interrupt();
}

#[embassy_nrf::pac::interrupt]
fn TIMER1() {
    // Minimal TIMER ISR: set flag, pend RADIO ISR (R9)
    ESB_ISR.on_timer_interrupt();
    // Internally: cortex_m::peripheral::NVIC::pend(Interrupt::RADIO)
}
```

**Single-ISR-context architecture** (R9): All ESB state machine logic runs in the RADIO ISR. The TIMER ISR is minimal (clear events, set flag, pend RADIO ISR). This avoids ISR-to-ISR synchronization issues.

ISR priorities: RADIO and TIMER at P0 (highest) for correct timing.

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| ISR priority conflict with MPSL | Medium | High | In exclusive mode, RADIO/TIMER at P0 is fine. In MPSL mode, MPSL owns RADIO at P0 and dispatches to ESB via signal callback — ISR is NOT used directly |
| ISR handler too slow (stack overflow) | Low | Critical | Keep ISR handler minimal: read events, call `ptx_step`/`prx_step`, write registers, return. No allocation, no logging |
| Compiler inlining breaks ISR timing | Low | High | Mark ISR-critical functions `#[inline(always)]`; verify with `cargo-asm` if needed |

**Rollback**: Use esb-ng's approach of doing everything inside the ISR (no separation) if pure-function approach causes timing issues.

---

### M8: Embassy Async API (2-3 days)

**File**: `src/async_driver.rs`

**Public API**:
```rust
// Static buffer anchors ISR state (R12 fix)
static ESB_BUF: EsbBuffer<4, 252> = EsbBuffer::new();

// Init: try_split takes &'static self, returns 'static handles
let (ptx, isr_handle) = ESB_BUF.try_split(timer, radio, &addresses, Config::default())?;
// timer: embassy_nrf::peripherals::TIMER1 → EsbTimerHandle (R1)
// radio: embassy_nrf::peripherals::RADIO → pac::RADIO

// PTX usage
ptx.send(pipe, &payload).await?;           // send with ACK (default)
ptx.send_no_ack(pipe, &payload).await?;    // send without ACK (R15)
let ack = ptx.try_receive();                // check ACK payload
let dropped = ptx.max_attempts_reached();   // check if packet was dropped (R15)

// PRX usage
let (prx, isr_handle) = ESB_BUF.try_split(timer, radio, &addresses, Config::default())?;
prx.start_listening();
let packet = prx.receive().await;           // wait for packet
prx.send_ack_payload(pipe, &response).await?;  // queue ACK payload

// Cleanup (needed for Phase 7 BLE/ESB switching)
ptx.stop();  // disable RADIO, release resources (R15)

// Suspend/restore (core API, used by MPSL timeslots and BLE/ESB hot-switch)
let state = ptx.suspend()?;          // save PID, disable RADIO, stop timer
// ... MPSL runs BLE, or user switches mode ...
ptx.restore(&state, &addresses)?;    // full re-init, restore PID, resume
```

**ISR-to-async bridging** (R14): ISR calls `Channel::try_send()` to `rx_queue`, which triggers the Embassy waker via `CriticalSectionRawMutex`. The `receive().await` is woken automatically. No separate `Signal` needed.

**Verification**: Compiles with Embassy executor, API usability (<50 lines for a ping-pong example)

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| `send().await` blocks ISR from sending | Medium | High | `send()` queues to `tx_queue` and pends ISR via `NVIC::pend(RADIO)`. ISR picks up from queue. If busy, queue and retry on next idle |
| `receive().await` never wakes | Medium | Medium | ISR `Channel::try_send()` triggers waker via CriticalSectionRawMutex (R14) |
| Builder consumes timer+radio but ISR needs static access | Medium | Critical | `EsbBuffer::try_split(&'static self)` pattern — buffer is `static`, handles are `'static` (R12) |
| Missing methods for RMK integration | Low | Medium | `send_no_ack()`, `stop()`, `max_attempts_reached()` added to API (R15) |

**Rollback**: Provide blocking API (`send_blocking()`, `receive_blocking()`) as fallback if async integration is blocked.

---

### M8.5: Suspend/Resume API (1-2 days)

**File**: `src/suspend.rs`

**Why this is a core module**: MPSL timeslots and BLE/ESB hot-switching both need the same capability — cleanly save ESB state, release RADIO, then restore and continue. This is NOT a feature-gated add-on; it's a fundamental API that both exclusive-mode and MPSL-mode users need.

**Reference**: `too1/ncs-esb-ble-mpsl-demo/app_esb.c` (`app_esb_suspend`/`app_esb_resume`), `inductivekickback/ncs_ble_esb_demo/proprietary_rf.c` (`esb_get_pid`/`esb_set_pid`)

**Key types**:
```rust
/// Saved state that survives across suspend/resume cycles.
/// Must be stored by the caller (MPSL timeslot handler or BLE/ESB switch code).
#[derive(Clone, Copy)]
pub struct EsbSavedState {
    pub pid: [u8; 8],              // PID per pipe (2-bit counters, persisted)
    pub current_pipe: u8,           // Active pipe for PTX
    pub retransmit_count: u8,       // Current retry count
    pub last_crc: [u16; 8],        // CRC per pipe for duplicate detection
    pub state: SavedProtocolState,  // Idle, MidTx, MidRx
}

pub enum SavedProtocolState {
    Idle,                           // Safe to suspend anytime
    MidTransaction { attempt: u8 }, // Mid-retransmit; save attempt count
}
```

**Core API on EsbPtx/EsbPrx** (fix C3 — timing constraints):
```rust
impl EsbPtx {
    /// Try to suspend — returns Err(Busy) if state machine is mid-transaction.
    /// Use in MPSL timeslot end callback (cannot afford to wait).
    pub fn try_suspend(&self) -> Result<EsbSavedState, Error>;

    /// Async suspend — waits for current transaction to complete (or timeout) then suspends.
    /// Use for user-initiated mode switching (can afford to wait).
    pub async fn suspend(&self) -> Result<EsbSavedState, Error>;

    /// Full ESB re-init + restore saved state.
    /// RADIO power-cycled, addresses reconfigured, PID restored.
    pub fn restore(&mut self, state: &EsbSavedState, addresses: &EsbAddresses) -> Result<(), Error>;
}
```

**`suspend()` internal timing**:
1. Disable RADIO IRQ (block new transactions)
2. Check if state machine is Idle → if yes, save immediately
3. If mid-transaction: `try_suspend` returns `Err(Busy)`; `suspend().await` waits for ISR to complete current transaction (via Signal notification)
4. Timeout mechanism: if wait >1ms (abnormal), force disable RADIO + drop current packet
```

**suspend() sequence** (matches too1 `app_esb_suspend`):
1. Disable RADIO IRQ (`NVIC::disable_irq(RADIO)`)
2. Set RADIO SHORTS = 0, trigger TASKS_DISABLE, spin-wait for EVENTS_DISABLED
3. Stop TIMER
4. Clear all RADIO interrupts (`INTENCLR = 0xFFFFFFFF`)
5. Save PID from `last_pid[]`, save CRC from `last_crc[]`, save attempt count
6. Clear pending RADIO IRQ

**restore() sequence** (matches too1 `app_esb_resume`):
1. RADIO power cycle: `POWER = Off; POWER = On`
2. Full `EsbRadio::init()` (register setup, CRC config, shorts)
3. Configure addresses
4. Restore PID to `last_pid[]`, restore CRC to `last_crc[]`
5. Pull pending TX from queue if any
6. Re-enable RADIO IRQ

**Host tests**: save/restore round-trip, PID persistence across 100 cycles

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Suspend during mid-transaction corrupts state | Medium | High | Only suspend from ISR context when state machine is in Idle. If mid-TX, complete the transaction first or drop the packet |
| PID lost on suspend causes duplicate rejection | Low | Critical | Explicitly save/restore `last_pid[pipe]`. Test with counter: 1000 packets across 100 suspend/resume cycles |
| RADIO power cycle timing | Low | Medium | too1 demo does immediate Off→On. Verify no settling time needed per PS |
| Restore re-init overhead | Low | Low | ~20 register writes ≈ a few microseconds. Acceptable for timeslot transitions |

---

### M9: Hardware Verification — Exclusive Mode (3-5 days)

**Hardware**: E104-BT5040U (PRX) + nice!nano (PTX), both nRF52840

**Step-by-step verification**:

| Step | Test | Pass Criteria |
|------|------|---------------|
| 1 | Radio init | RTT/defmt shows correct frequency, address, RX mode |
| 2 | Single PTX→PRX packet | Payload matches, zero corruption |
| 3 | ACK round-trip | PTX receives ACK payload from PRX |
| 4 | 1000-packet stream | 0% loss at 1m, <1% at 5m |
| 5 | Retransmit | Power off PRX → PTX reports MaximumAttempts; power on → recovery |
| 6 | Multi-pipe (2 pipes) | Both pipes received correctly |
| 7 | Latency (GPIO + logic analyzer) | <500us end-to-end at 2Mbps |
| 8 | Soak test (8h overnight) | <0.01% loss, zero panics |

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| No communication at all (step 2 fails) | Medium | Critical | Add diagnostic counters (RADIO events, CRC count, DMA pointer state) at every layer. Compare with known-working Gazell setup |
| Packet corruption (payload mismatch) | Low | High | Verify CRC config matches; check byte ordering (little-endian on nRF52); test with fixed payload pattern |
| ACK timeout too aggressive | Medium | Medium | Increase `ack_timeout` from 120us to 250us if initial tests fail; this is a config tuning issue |
| Multi-pipe address collision | Low | Medium | Verify prefix addresses are unique per pipe; test pipes one at a time first |
| Soak test reveals memory corruption | Low | Critical | Run with defmt logging enabled; track buffer pool in-flight count; check for stack overflow with stack canary |

**Rollback**: If ESB doesn't work at all, compare RADIO register dumps between working Gazell and nrf-esb to identify misconfiguration.

---

### M10: MPSL Timeslot Adapter (5-7 days) — HARD REQUIREMENT

**File**: `src/mpsl_timeslot.rs`

**Status**: This is a hard requirement, not optional. BLE+ESB concurrent operation is a core use case for RMK.

**References**:
- `too1/ncs-esb-ble-mpsl-demo/timeslot_handler.c` — Low-level timeslot handler with manual ESB suspend/resume
- `inductivekickback/ncs_ble_esb_demo/timeslot.c` — Radio Notification approach, PID persistence pattern
- `nrfconnect/sdk-nrf/samples/esb/esb_ptx_ble/` — Nordic's official ESB+BLE sample
- RMK `nrf52840_radio_switch_poc` — Proven dynamic RADIO ISR dispatch

#### Key Findings from Reference Analysis

1. **RADIO power cycling between every timeslot**: `RADIO.POWER = Off; RADIO.POWER = On` to clear BLE state. Both demos do this on SIGNAL_START.

2. **Full ESB re-initialization per timeslot** (not lightweight suspend/resume): On timeslot end → `esb_disable()` + manual RADIO cleanup. On timeslot start → full `esb_init()` + address config + PID restore.

3. **PID persistence across timeslots**: `esb_get_pid()` before suspend, `esb_set_pid()` after resume. The 2-bit PID sequence counter is used for duplicate detection; losing it causes the receiver to reject valid packets.

4. **MPSL API serialization**: All `mpsl_timeslot_*` calls must go through a single cooperative context. NOT reentrant.

5. **TIMER0 owned by MPSL**: During timeslots, MPSL pre-configures TIMER0 (1MHz mode). Applications can use CC channels for safety margin and extension requests but must not reconfigure the mode. **ESB must use TIMER1/2 for protocol timing**.

6. **HFCLK configuration**: Two options — `NO_GUARANTEE` (HFCLK may be off, lower power, higher jitter) or `XTAL_GUARANTEED` (32MHz crystal always on, better RF). For keyboard use, `XTAL_GUARANTEED` is safer.

7. **ZLI (Zero Latency IRQ) workaround**: MPSL callbacks run at priority 0 (non-maskable). Cannot call kernel functions from ZLI context. Must defer to lower-priority IRQ via `NVIC_SetPendingIRQ()`. In Embassy/async context, use `Signal<RawMutex, _>` to notify the async task.

8. **Timeslot extension**: Can request extension during a timeslot to keep RADIO longer. CC[0] triggers extension request before timeslot expires. Useful for burst TX.

#### Architecture

```
[Session Open] ──→ [Request Earliest Timeslot]
                        │
                   SIGNAL_START ──→ RADIO power cycle ──→ ESB full init ──→ App callback
                        │                                                        │
                   (ESB runs normally within timeslot)                            │
                        │                                                        │
                   SIGNAL_TIMER0 ──→ request extension OR end timeslot           │
                        │                                                        │
                   SIGNAL_EXTEND_SUCCEEDED ──→ continue ESB                      │
                        │                                                        │
                   (timeslot ending) ──→ save PID ──→ ESB disable ──→ RADIO cleanup
                        │
                   SIGNAL_BLOCKED/CANCELLED ──→ request new timeslot
                        │
                   SIGNAL_SESSION_IDLE ──→ request new timeslot
```

**Key types**:
```rust
pub struct TimeslotConfig {
    pub slot_length_us: u32,     // 5000-10000us (5-10ms for keyboard)
    pub timeout_us: u32,         // 1000000us (1s request timeout)
    pub hfclk: HfclkConfig,     // NoGuarantee or XtalGuaranteed
    pub priority: TimeslotPriority, // Normal or High
}

pub enum TimeslotSignal {
    Start,
    Timer0,
    ExtendSucceeded,
    ExtendFailed,
    Radio,
    Blocked,
    Cancelled,
    SessionIdle,
    Overstayed,
}
```

**Open questions (resolved during implementation)**:
1. Does `nrf-mpsl` Rust crate expose `mpsl_timeslot_*` functions? If not, need to add FFI bindings.
2. Can we use Embassy's `Signal` instead of `NVIC_SetPendingIRQ` for ZLI deferral?
3. What's the minimum practical timeslot length for a keyboard scan + TX cycle? (~5ms estimated)

**RMK PRX deployment strategy** (fix C4):
- **Recommended**: Dongle (PRX) runs exclusive mode — USB-only, no BLE needed. Keyboard (PTX) runs MPSL timeslot mode — needs BLE + ESB coexistence.
- **If dongle also needs BLE**: Requires synchronized timeslot or beacon mode (PRX and PTX communicate in agreed time windows). This significantly increases complexity — defer to separate milestone after M10.

**HW verification**:
- BLE advertises (visible on nRF Connect) + ESB packets succeed during timeslots
- Latency comparison: timeslot mode vs exclusive mode
- Blocked/cancelled timeslot recovery (stress test: continuous BLE scan while ESB TX)

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| `nrf-mpsl` doesn't expose timeslot API | Medium | Critical | Check nrf-mpsl-sys bindings; add FFI bindings ourselves if missing |
| ZLI context + Embassy async conflict | High | High | Use `Signal<RawMutex, TimeslotSignal>` (lock-free) to notify async task from ZLI. Don't call any Embassy API from ZLI |
| Timeslot too short for ESB TX+ACK cycle | Medium | High | Calculate: TX ramp-up (140us) + payload TX (128us @2Mbps for 32 bytes) + ACK wait (260us) = ~530us. 5ms slot gives 9+ transactions |
| Full ESB re-init overhead per timeslot | Medium | Medium | ~20 register writes ≈ a few microseconds. Measured in HW to confirm |
| BLE connection interval conflict | Medium | Medium | Set BLE interval to 30-100ms with slave latency 2-3. ESB gets ~20-90ms between connection events |
| PID loss causes duplicate rejection | Low | High | Save/restore PID in `EsbSavedState`. Test with a counter: send 1000 packets across 100 timeslot boundaries |
| PRX mode in timeslots unreliable | High | High | PRX must listen for entire timeslot. Consider making dongle = PTX (initiates) and keyboard = PRX (responds) as RMK already does |

**Rollback**: If MPSL timeslot integration fails, Phase 5A (exclusive mode) still works as a standalone ESB replacement for Gazell. Phase 7 can be deferred.

---

### M11: RMK Integration (2-3 days)

**File in RMK**: `rmk/src/split/esb.rs` (mirrors `gazell.rs`)

- Implement `SplitReader`/`SplitWriter` for `EsbPtx`/`EsbPrx`
- Add `RadioMode::Esb = 3` to `radio_dispatch.rs`
- Replace `rmk-gazell-sys` dependency with `nrf-esb`
- Add `"esb"` connection type to `rmk-macro` codegen
- ISR bridge: extend dynamic RADIO dispatch (`AtomicU8` selector) to support ESB mode

**Risk Analysis**:
| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| RMK split message format incompatible with ESB payloads | Low | Medium | ESB max payload=252 bytes; RMK `SplitMessage` is much smaller. No size issue |
| Codegen (`rmk-macro`) needs updates for ESB connection type | Medium | Medium | Add `"esb"` connection type alongside `"gazell"` in codegen. Same pattern, different driver |
| ISR bridge conflict with existing BLE/Gazell bridges | Medium | High | Follow Phase 4's `bind_interrupts!` avoidance pattern. Use `#[pac::interrupt]` manual ISR + `unsafe impl Binding` |

**Rollback**: Keep `rmk-gazell-sys` as fallback; ESB integration behind feature gate `wireless_esb`.

---

## Dependency Graph

```
M0 ─→ M1 ─→ M2 ─→ M3 ─→ M4 ─→ M5 ─→ M6 ─→ M7 ─→ M8 ─→ M8.5 ─→ M9 ─→ M10 ─→ M11
              └─────┘              └────────────┘
         (radio + timer       (PTX + PRX state
          can parallel)        machines can parallel)
```

## Risk Register (Global)

| Risk | Impact | Probability | Mitigation |
|------|--------|-------------|------------|
| ISR latency misses radio events | Critical | Low | RADIO/TIMER at P0 priority; minimal ISR handler |
| nrf-pac 0.3 silent API changes | Medium | Medium | Test each register operation individually; compare with working Gazell register dumps |
| Buffer pool DMA use-after-free | Critical | Low | "In-flight" tracking; only deallocate after ISR confirms RADIO idle |
| MPSL timeslot API unavailable in Rust | High | Medium | Phase 5A works standalone; Phase 7 can be deferred; add FFI bindings ourselves |
| ESB timing incompatibility with keyboard scan rate | Medium | Low | ESB TX at 2Mbps is ~10x faster than keyboard scan needs; no realistic conflict |

## Timeline

| Milestone | Duration (Optimistic) | Duration (Realistic) | Cumulative (Realistic) |
|-----------|----------------------|---------------------|----------------------|
| M0-M4: Core infrastructure | 5-8 days | 8-12 days | 12 days |
| M5-M7: Protocol + ISR | 5-7 days | 6-8 days | 20 days |
| M8: Async API | 2-3 days | 2-3 days | 23 days |
| M8.5: Suspend/Resume | 1-2 days | 2-3 days | 26 days |
| M9: HW verification (exclusive) | 3-5 days | 5-10 days | 36 days |
| **Phase 5A complete** | **~25 days** | **~30-36 days** | |
| M10: MPSL timeslot (hard req) | 5-7 days | 7-14 days | 50 days |
| M11: RMK integration | 2-3 days | 2-3 days | 53 days |

> Note: Hardware debugging (M9) and MPSL integration (M10) are the largest uncertainty sources. If exclusive mode works first try, it could be much faster; if there are subtle timing bugs, it could far exceed estimates.

## Verification Checklist (per milestone)

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --target thumbv7em-none-eabihf -- -D warnings`
- [ ] `cargo test` (host, for applicable milestones)
- [ ] `cargo check --target thumbv7em-none-eabihf`
- [ ] `cargo doc` no warnings
