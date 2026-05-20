# Review Fix Backlog After M10 First Pass

Created: 2026-05-19

Context: This captures review conclusions for `embassy-nrf-esb` before M10 first-pass work is complete. Do not execute this as the primary roadmap yet. Revisit after M10 first-pass development, then split into small independently verifiable fixes.

## Scope Boundary

This file is only the review-fix backlog for current implemented behavior. It intentionally separates review fixes from later product/integration work.

Review fixes are about correctness, safety, API clarity, and avoiding misleading claims in what already exists.

Later development items such as HID dongle, RMK frame format, pairing/static binding, channel switching, BLE+ESB end-to-end validation, and full Embassy publishing polish should remain in the original roadmap unless explicitly pulled forward.

## Confirmed Review Findings

### README Gazell Wording

`README.md` currently says this is a drop-in replacement for Nordic Gazell. That is too strong.

Current implementation is ESB-compatible, not Gazell-compatible. Gazell adds hopping/timeslot/host-id protocol behavior above ESB. This crate will not talk to an existing Gazell PRX dongle.

Suggested wording:

```md
Drop-in replacement for Nordic ESB; can replace Gazell when both sides are migrated.
```

### MPSL Timeslot Duplicates Main State Machines

`src/mpsl_timeslot.rs` contains independent PTX/PRX protocol state machines with phase enums and direct RADIO register handling, bypassing `PtxStateMachine` and `PrxStateMachine` in `src/state_machine.rs`.

This duplicates PID progression, duplicate detection, ACK payload handling, NoAck behavior, and retransmit behavior. Long-term this should be unified so MPSL timeslots reuse the main state machines or a shared protocol core.

Short-term stopgap fixes should be done before the larger unification.

### MPSL PRX PID/CRC State Clearing Bug

In `src/mpsl_timeslot.rs`, PRX TIMER0 handling creates a fresh `EsbRadio::new(pac::RADIO)` and then saves PID/CRC state from it.

Because `EsbRadio::new()` initializes `last_pid` and `last_crc` to zero, this clears the PRX cross-slot duplicate detection state.

Fix before relying on PRX-in-timeslot stability.

### MPSL Header Bit Writes Bypass Helper

MPSL code manually writes header PID/NoAck bits, e.g. direct `tx_buf[dma_off + 1]` updates.

This should use `EsbHeader::set_pid()` and `set_no_ack()` to avoid silent drift if header layout changes.

### MPSL Uses Single Static Buffers

`PTX_BUFS` and `PRX_BUFS` are single global 256-byte buffer sets, unlike the main path's configurable `PacketPool<N, SIZE>`.

This is acceptable for smoke/perf examples, but not for a general MPSL API. If MPSL remains public, users need to pass a pool or a handle that owns buffers.

### MPSL Power Cycle Per Slot Needs Measurement

Each `SIGNAL_START` currently performs full `radio.power_cycle()` plus `radio.init()`.

This has worked in measured 6 ms slots, but 2 ms slots and BLE long connection events need timing data. Optimize only after instrumentation unless a concrete miss is observed.

### OVERSTAYED Must Not Panic

`OVERSTAYED` currently panics in MPSL paths. This is a real MPSL signal, not an impossible condition.

Convert it to observable counters plus safe stop/end behavior. A keyboard firmware must not panic because a slot overstayed.

### MPSL Free Functions Are Re-entrant

`run_ptx_slots()` and `run_prx_slots()` are free functions backed by global static state. Concurrent calls can overwrite state/waker/request.

Add an owning handle or at least an atomic busy guard that rejects re-entry.

### Missing Host-side Unit Tests

There are no `#[cfg(test)]`/`#[test]` host tests despite `state_machine.rs` being designed around a pure event machine.

Add tests for header bit operations, address reversing/packing, config validation, PID/CRC duplicate transitions, and any protocol helpers that can be made PAC-free.

## Additional Review Findings

### UnsafeCell State Machine Access Is Not Sufficiently Protected

`EsbPtx` and `EsbPrx` store state machines in `UnsafeCell` with `unsafe impl Sync`. Several `&self` methods access state machine internals from task context while RADIO/TIMER ISR can also mutate them.

Examples include `set_pipe()`, `state()`, `start_listening()`, `stop()`, and PRX/PTX ISR entrypoints.

This should be guarded by masking the relevant IRQs, using a clear critical-section helper, or restructuring so task context communicates via atomics/channels while ISR owns mutable state.

### PRX ACK Payload Pipe Parameter Is Not Enforced

`send_ack_payload(pipe, payload)` records a pipe in the header, but the PRX state machine dequeues the next ACK payload without filtering by the current RX pipe.

In multi-pipe scenarios, an ACK payload intended for pipe 1 can be consumed by a pipe 0 packet.

Fix by making ACK queues per-pipe or adding `try_dequeue_tx_for_pipe(pipe)` semantics.

### PTX Pipe Is Global State Rather Than Packet Metadata

`set_pipe()` changes a global current TX pipe. Queued packets do not carry their intended pipe.

This works in simple single-task examples, but can send queued packets on the wrong pipe if tasks interleave or the queue backs up.

Add `send_to(pipe, payload)` and store pipe as packet metadata. Keep `set_pipe()` only as a demo/convenience API if necessary.

### Fallback ACK Buffer Alignment

The fallback empty ACK buffer should be explicitly word-aligned for EasyDMA. Main `Packet` is aligned, but the fallback static wrapper is not.

Use `#[repr(C, align(4))]` on the fallback ACK wrapper.

### Duplicate Detection Needs Valid Bits

`last_pid`/`last_crc` initialize to zero and duplicate detection compares only `crc + pid`.

If the first packet on a pipe has `pid = 0` and `crc = 0`, it can be treated as duplicate. Low probability but protocol-incorrect.

Add `last_valid: [bool; NUM_PIPES]` and only compare after a valid previous packet exists. Apply consistently to MPSL paths if they keep duplicate logic.

### Unbounded RADIO Disable Wait

`EsbRadio::stop()` waits forever for `EVENTS_DISABLED` after `TASKS_DISABLE`.

Bound this wait and return an error or perform a recovery path. Infinite waits can deadlock keyboard/dongle firmware.

### Public Constructors Panic on Invalid Config

`EsbPtx::new()` and `EsbPrx::new()` call `config.validate().expect(...)`.

Library constructors should return `Result<Self, Error>` so firmware can handle configuration errors without panic.

### `payload_length` Field Is Misleading

`EsbConfig::payload_length` is validated but not used to set the actual RADIO max length or enforce sends. Current code uses dynamic payload lengths and RADIO `MAXLEN` is fixed to the ESB max payload.

Delete it or wire it through consistently.

### Critical-section Feature Conflict Is Not Guarded

`mpsl` and `_cs-cortex` are documented as mutually exclusive, but `cargo check --features nrf52840,mpsl,_cs-cortex` currently succeeds.

Add a compile-time guard.

### HFCLK Ownership Needs Clear API/Docs

Some examples manually start HFCLK; MPSL examples hold it through MPSL. The library does not own the clock.

Document that exclusive mode currently requires the application to provide HFCLK, or introduce a clock guard/feature later.

## RMK / Keyboard Integration Notes

These are not required for the ESB crate review fixes, but are important before RMK integration.

- Full Gazell hopping/host-id/pairing is not needed for the immediate replacement if both sides migrate.
- Do not use Nordic default ESB addresses for real keyboards.
- RMK payloads should include at least protocol version, device id, sequence number, report type, and payload length.
- PRX/RMK layer must deduplicate by device id plus sequence number to avoid duplicate key reports after retries.
- Static binding is enough for first pass; dynamic pairing can come later if product requirements demand it.
- Fixed RF channel is acceptable for MVP; channel switch/scan can be postponed until interference data requires it.

## Review Fix Execution Plan After M10 First Pass

### Fix Batch 1: Documentation and Config Stopgap

Scope:

- README Gazell wording.
- Clarify ESB versus Gazell compatibility.
- Delete or correctly wire `payload_length`.
- Add `mpsl` versus `_cs-cortex` compile guard.
- Document HFCLK ownership.

Status:

- 2026-05-20: README ESB/Gazell wording and HFCLK ownership are documented. `payload_length` is wired as the configured max payload length for RADIO `MAXLEN`, PTX sends, NoAck sends, and PRX ACK payloads. The `mpsl` versus `_cs-cortex` compile guard already exists in `src/lib.rs`.

Verification:

- `cargo check --features nrf52840,_cs-cortex`
- `cargo check --features nrf52840,mpsl`
- `cargo check --features nrf52840,mpsl,_cs-cortex` must fail with clear error.
- `cargo fmt --check`

### Fix Batch 2: MPSL Stopgap Correctness

Scope:

- Convert `OVERSTAYED` panic to counters and safe termination.
- Fix PRX TIMER0 PID/CRC clearing.
- Add MPSL re-entry guard.
- Use `EsbHeader` helpers in MPSL header writes.

Status:

- 2026-05-20: `OVERSTAYED` panic removal, PRX TIMER0 PID/CRC preservation, MPSL header helper usage, and free-function re-entry guards are implemented. The re-entry guards return `Error::Busy` before opening a second session on the same global MPSL path.

Verification:

- Two-dongle MPSL PTX/PRX examples.
- Shorten slot or match time to trigger overstayed; confirm no panic.
- Confirm counters report overstayed/blocked/cancelled.

### Fix Batch 3: Rust Safety Boundary

Scope:

- Guard `UnsafeCell` state machine access from task context.
- Make constructors return `Result` instead of panic.
- Bound `radio.stop()` waiting behavior.
- Align fallback ACK buffer.

Status:

- 2026-05-20: `EsbRadio::stop()` uses a bounded wait and power-cycle recovery instead of an infinite spin. The fallback empty ACK buffer is explicitly 4-byte aligned. `EsbPtx::new()` and `EsbPrx::new()` now return `Result<Self, Error>` instead of panicking on invalid config. Broader `UnsafeCell` access guards are still pending.

Verification:

- Exclusive PTX/PRX examples still work.
- Stress test repeated state queries, start/stop, send loops.
- No deadlock or panic in 10-30 minute hardware run.

### Fix Batch 4: Protocol Correctness

Scope:

- Duplicate detection valid bit.
- Per-pipe ACK payload queue.
- Per-packet TX pipe metadata via `send_to(pipe, payload)`.
- Deprecate or document global `set_pipe()` limitations.

Verification:

- Two-dongle multi-pipe test.
- Pipe-specific ACK payloads do not cross pipes.
- Repeated/retransmitted packets do not produce duplicate application events.

### Fix Batch 5: Tests and CI Base

Scope:

- Host-side tests for `EsbHeader`, `EsbAddresses`, config validation, and pure duplicate/PID helpers.
- Add CI check matrix if project is ready.

Verification:

- `cargo test` for host-testable units, or a dedicated test crate if needed.
- `cargo check` feature matrix.

## Items To Avoid Mixing Into Review Fixes

Do not mix these into review-fix PRs unless the current milestone already requires them:

- HID dongle implementation.
- RMK frame finalization.
- Dynamic pairing.
- Channel hopping or channel scanning.
- BLE + ESB end-to-end coexistence validation.
- Full MPSL wrapper architecture rewrite.
- Power management policy.
- crates.io/community publishing polish beyond basic guardrails.

## Suggested Resume Point

After M10 first-pass development is complete, resume from Fix Batch 1 and Fix Batch 2 first. Then choose between Rust safety fixes and MPSL state-machine unification based on what M10 surfaced.
