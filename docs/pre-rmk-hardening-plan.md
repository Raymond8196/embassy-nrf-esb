# Pre-RMK Hardening Plan

Created: 2026-05-21

This plan captures the work to do before starting the RMK ESB adapter. The goal
is not to add product features yet. The goal is to make the ESB crate stable
enough that RMK can depend on it as a 2.4 GHz split transport without inheriting
unclear safety, timing, or API risks.

## Target State

- Exclusive ESB PTX/PRX is the stable path for the first RMK dongle prototype.
- MPSL timeslot support remains available for experiments, but is clearly marked
  as diagnostic/experimental until it has an owned API and better retry timing.
- Rust safety boundaries around ISR-owned state are explicit and consistently
  implemented.
- Host-side tests cover the pure protocol and buffer ownership behavior that RMK
  depends on.
- Hardware regression steps for two dongles are repeatable and documented.

## Current Baseline

- Exclusive ESB has per-packet pipe selection through `send_to()` and
  `send_no_ack_to()`.
- PRX ACK payload selection is pipe-filtered.
- Duplicate detection uses `valid + pid + crc`.
- `PacketPool` now separates allocation from queued visibility:
  `free -> tx_allocated -> tx_queued -> in_dma -> free`.
- Transport framing has a 5-byte header and a single-packet higher-level payload
  limit of 247 bytes.
- `accept_bound_frame()` covers the RMK central-side decode, static binding, and
  sequence-dedup flow.
- Two-dongle MPSL diagnostic runs show working multi-pipe ACK payloads, but
  recent repeats show occasional pipe 1 misses and diagnostic PTX timing gaps.

## Phase 1: Rust Safety Boundary

Scope:

- Audit every `UnsafeCell` access in `src/isr.rs`.
- Replace open-coded RADIO IRQ masking sequences with a small helper so task-side
  state-machine access has one consistent pattern.
- Re-check `suspend()` poll paths for the order of waker registration, state
  reads, and IRQ masking.
- Keep ISR entrypoints documented as ISR-only.
- Avoid large architecture changes in this phase.

Acceptance:

- No open-coded task-side `disable_radio_irq(); ... enable_radio_irq();` blocks
  remain where the helper can be used.
- Early-return paths cannot accidentally leave RADIO IRQ masked.
- Existing host tests and feature checks still pass.

Progress:

- 2026-05-21: Added a small RADIO IRQ mask guard in `src/isr.rs`. Temporary
  task-side state access now uses a shared helper. Suspend paths still keep
  RADIO IRQ masked on success until `restore()`, but busy/early-return paths are
  guarded by RAII drop behavior.
- 2026-05-21: Completed the `src/isr.rs` task-context `UnsafeCell` audit for
  the exclusive ESB path. ISR entrypoints remain ISR-only; task-side
  state-machine mutation or reads are either IRQ-masked or limited to
  ISR-pending operations that do not touch mutable protocol state. Async
  `suspend()` now clears `suspend_requested` if the future is cancelled before
  suspension completes.

## Phase 2: Exclusive ESB API Polish

Scope:

- Treat `EsbPtx`, `EsbPrx`, `PacketPool`, and `transport` as the stable RMK
  foundation.
- Clarify `set_pipe()` as a default-pipe convenience API; recommend `send_to()`
  for multi-task or multi-pipe firmware.
- Review public errors (`InvalidParam`, `Busy`, `TxFull`) and document where RMK
  should map them to diagnostics versus retry policy.
- Ensure examples are clearly separated into core examples, USB diagnostics, and
  MPSL diagnostics.

Acceptance:

- RMK adapter code should not need to infer hidden behavior from examples.
- Payload sizing and pipe-binding requirements are documented in one place.

Progress:

- 2026-05-21: Public PTX/PRX API docs now document `send_to()` /
  `send_no_ack_to()` as the preferred multi-pipe APIs, clarify `set_pipe()` as a
  simple default-pipe convenience, document pipe-filtered ACK payload behavior,
  and list the expected `TxFull` / `InvalidParam` error cases for queueing
  methods.

## Phase 3: MPSL Risk Containment

Scope:

- Mark `mpsl_timeslot` as experimental/diagnostic in module docs and verification
  docs.
- Document current limitations:
  - static global state;
  - free-function sessions;
  - fixed static buffers;
  - protocol logic duplicated from the main state machines;
  - diagnostic PTX path lacks full ACK timeout/retry behavior;
  - BLE coexistence still needs scheduling work.
- Add cheap counters only if they directly explain hardware runs.

Acceptance:

- MPSL remains useful for hardware experiments.
- MPSL is not presented as the API RMK should use for the first dongle
  prototype.

Progress:

- 2026-05-21: `mpsl_timeslot` module docs now explicitly mark the free-function
  timeslot helpers as experimental/diagnostic and list the current static-state,
  fixed-buffer, duplicated-protocol, retry-timing, and BLE-scheduling
  limitations.
- 2026-06-01: Added explicit MPSL coexistence profiles and PRX/PTX diagnostic
  config structs so the 3-mode examples choose a named profile instead of
  carrying raw slot constants. Extracted pure MPSL diagnostic helpers for PID
  advance, pipe-mask round-robin, counter packet encode/decode, per-pipe deltas,
  and bounded spin loops; these are now host-tested. PTX diagnostic RADIO
  disable waits now use a bounded helper instead of unbounded spins, and
  `SignalCounters` reports bounded disable waits that hit the spin limit.

## Phase 4: Host Test Expansion

Scope:

- Add pure host tests for malformed transport frames, oversize payloads, binding
  mismatch, duplicate sequence drops, and packet-pool RX lifecycle.
- Expand config edge-case tests where behavior affects RMK payload sizing or
  retransmission.
- Prefer extracting small pure helpers over testing PAC-register code directly.

Acceptance:

- `cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840`
  remains the fast pre-hardware gate.
- Tests cover the assumptions RMK will rely on before any keyboard-specific code
  exists.

Progress:

- 2026-05-21: Added host coverage for RX DMA claim behavior and buffer content
  visibility. Full `rx_complete()` queue lifecycle still needs either a host
  critical-section provider or a different test seam, because
  `embassy_sync::Channel` requires critical-section symbols when linked on the
  host target.
- 2026-05-21: Rechecked the full `rx_complete()` queue lifecycle test. It still
  links against `embassy_sync::Channel` critical-section symbols on the default
  host gate, so it remains intentionally unlanded until the crate has an
  explicit host critical-section provider or a PAC-free test seam.
- 2026-06-01: Added an x86_64 dev-only `critical-section/std` provider, which
  unblocks host tests that exercise `embassy_sync::Channel`. Host coverage now
  includes the full `rx_complete()` queue/release lifecycle, pipe-filtered TX
  dequeue skipping allocated or wrong-pipe slots, transport trailing-byte
  decode behavior, and unbound/out-of-range route rejection.
- 2026-06-01: Host coverage increased to 40 tests after adding pure MPSL
  diagnostic helper tests.

## Phase 5: Hardware Regression Procedure

Scope:

- Document or script the two-dongle workflow:
  - build release examples;
  - convert ELF to HEX;
  - generate unsigned DFU zips;
  - flash PRX and PTX;
  - capture PTX USB CDC output;
  - record pass/fail criteria.
- Keep the first procedure focused on exclusive ESB PRX/PTX traffic, because
  this is the RMK prototype baseline.
- Keep `mpsl_prx_in_slot` + `mpsl_ptx_in_slot` as the second diagnostic
  procedure for timeslot regression checks.

Acceptance:

- A future protocol change can be verified without reconstructing command
  history from memory.
- Hardware results include exact firmware pair, ports, and output lines.

Progress:

- 2026-05-21: Ran an exclusive ESB ACK echo smoke on two nRF52840 dongles with
  `prx_usb` + `ptx_ack_echo`. PRX reported `rx=1200 lost=0 loss=0.0%`; PTX
  reported `tx=1200 ack_rx=1199` during a 12 second USB CDC capture. This is a
  short smoke, not the 30 minute C2 acceptance run.
- 2026-06-01: Added `make check-no-hw` and split targets for host tests,
  exclusive examples, MPSL examples, feature-conflict checking, and targeted
  formatting/whitespace checks. This gives protocol and API changes a single
  no-dongle pre-hardware gate.

## Phase 6: RMK Interface Recheck

Scope:

- Pull or inspect latest RMK before writing adapter code.
- Reconfirm:
  - `SplitMessage` location and visibility;
  - `SplitReader` / `SplitWriter` signatures;
  - `SPLIT_MESSAGE_MAX_SIZE`;
  - postcard serialization pattern used by existing split drivers.
- Confirm `SPLIT_MESSAGE_MAX_SIZE <= 247` for single-packet ESB transport, or
  explicitly decide on fragmentation before implementation.

Acceptance:

- RMK ESB adapter implementation can start from a known trait and payload
  contract.
- This crate remains independent of RMK types.

Progress:

- 2026-05-21: Rechecked local RMK checkout at
  `822e706640c54a72415b51361e4c890ec13b362a`. `SplitMessage` remains
  `pub(crate)` in `rmk/src/split/mod.rs`; `SplitReader` and `SplitWriter`
  remain `pub(crate)` in `rmk/src/split/driver.rs`; writers still serialize via
  `postcard::to_slice()` into `[u8; SPLIT_MESSAGE_MAX_SIZE]`. A temporary RMK
  integration test printed `SPLIT_MESSAGE_MAX_SIZE = 20`, so it fits inside the
  current 247-byte single-packet ESB transport payload budget.

## Recommended Execution Order

1. Phase 1: Rust safety helper and `UnsafeCell` audit.
2. Phase 2: Exclusive ESB API polish.
3. Phase 4: Host tests for the protocol and buffer lifecycle assumptions.
4. Phase 6: Latest RMK interface recheck.
5. Phase 5: Exclusive ESB hardware regression procedure.
6. Phase 3: MPSL experimental boundary documentation.

## Deferred Until After RMK Prototype

- Dynamic pairing.
- Channel hopping or scanning.
- Encryption.
- Full MPSL owned-wrapper rewrite.
- BLE + ESB product-mode scheduler.
- HID dongle polish beyond the minimum needed to prove key events.
