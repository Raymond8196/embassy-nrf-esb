# Review Fix Backlog (Completed)

Created: 2026-05-19
All batches completed: 2026-05-21

This was the review-fix backlog after M10 first pass. All items below are
done. Kept as a record of what was reviewed and fixed. See
`../current-status.md` for current work and
`../single-engine-convergence-plan.md` for the ongoing engine convergence.

## Completed Batches

| Batch | Scope | Status |
|-------|-------|--------|
| 1 — Docs & Config | README ESB/Gazell wording, HFCLK ownership, `payload_length` wired, `mpsl`/`_cs-cortex` compile guard | Done 2026-05-20 |
| 2 — MPSL Correctness | OVERSTAYED panic → counters, PRX TIMER0 PID/CRC preservation, MPSL re-entry guard, `EsbHeader` helpers | Done 2026-05-20 |
| 3 — Rust Safety | `EsbRadio::stop()` bounded wait + power-cycle recovery, fallback ACK 4-byte aligned, `EsbPtx::new`/`EsbPrx::new` return `Result` instead of panic, `UnsafeCell` IRQ-masked access | Done 2026-05-21 |
| 4 — Protocol | Per-pipe dup-detection valid bits, per-pipe ACK payload filtering, `send_to(pipe, payload)` per-packet TX pipe metadata, `PacketPool` TX ownership tightened | Done 2026-05-20 |
| 5 — Tests & CI | Host tests for `EsbHeader`, `EsbAddresses`, config validation, `PacketPool`, dup detection, transport framing, static binding, sequence dedup; CI matrix | Done 2026-05-21 |

## Notable Verification Points

- 2026-05-20 hardware regression: `mpsl_prx_in_slot` + `mpsl_ptx_in_slot` on two nRF52840 dongles — `pipe=0 tx=450 ack=450 ackpl=450 ctr=450`, `pipe=1 tx=450 ack=450 ackpl=450 ctr=450`.
- RMK note (2026-05-21): `SplitMessage` (postcard-serialized) max size is 20 bytes; the ESB transport must carry serialized `SplitMessage` bytes, not invent a parallel frame.
