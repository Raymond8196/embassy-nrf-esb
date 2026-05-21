# RMK ESB Integration Notes

Created: 2026-05-20

This document records the intended RMK integration shape based on the local RMK
workspace at `/home/qlg/wkspaces/rmk`.

## Local RMK Interface Facts

Current local RMK split contracts:

- `rmk/src/split/driver.rs` defines crate-private traits:
  - `SplitReader::read() -> Result<SplitMessage, SplitDriverError>`
  - `SplitWriter::write(&SplitMessage) -> Result<usize, SplitDriverError>`
- `rmk/src/split/mod.rs` defines crate-private `SplitMessage`.
- `SPLIT_MESSAGE_MAX_SIZE = SplitMessage::POSTCARD_MAX_SIZE + 4`.
- Existing Gazell and BLE split paths serialize `SplitMessage` with postcard into
  fixed buffers of `SPLIT_MESSAGE_MAX_SIZE`.

Consequence: `embassy-nrf-esb` should not depend on RMK directly. The adapter
that implements `SplitReader` / `SplitWriter` belongs in RMK, while this crate
should provide the ESB driver and generic framing helpers.

## Layering

```text
RMK split manager
  └── RMK ESB adapter       (lives in RMK)
        ├── postcard SplitMessage serialization
        ├── static binding table: device id -> pipe/address
        ├── retry/dedup policy
        └── embassy-nrf-esb
              ├── EsbPtx / EsbPrx
              ├── PacketPool
              └── transport::{encode_frame, decode_frame, SequenceTracker, StaticBindingTable}
```

## Frame Format

Use `src/transport.rs`:

```text
byte 0: protocol_version
byte 1: device_id
byte 2: sequence_number
byte 3: flags (FLAG_ACK, FLAG_RETRANSMIT; other bits reserved)
byte 4: payload_len
byte 5..: postcard-serialized RMK SplitMessage bytes
```

Payload length requirement:

```text
ESB payload_length >= TRANSPORT_HEADER_LEN + SPLIT_MESSAGE_MAX_SIZE
SPLIT_MESSAGE_MAX_SIZE <= MAX_TRANSPORT_PAYLOAD_LEN
```

The current `EsbConfig::default().payload_length` is 32, so RMK ESB builds
should explicitly set a larger payload length if RMK's serialized split message
can occupy the Gazell-era maximum.

Use `transport::required_esb_payload_len(SPLIT_MESSAGE_MAX_SIZE)` and
`transport::fits_esb_payload(config.payload_length, SPLIT_MESSAGE_MAX_SIZE)` in
the RMK adapter or board config validation to catch this before runtime.
Because ESB's maximum payload is 252 bytes and this transport header is 5 bytes,
one framed packet can carry at most 247 bytes of postcard-serialized
`SplitMessage`. If RMK's `SPLIT_MESSAGE_MAX_SIZE` grows beyond that, the ESB
adapter needs fragmentation or a smaller RMK split message shape instead of
silently truncating or relying on runtime send failures.

## MVP Binding Policy

Use static binding first:

| RMK concept | ESB mapping |
|-------------|-------------|
| peripheral id | `device_id` |
| static binding slot | ESB pipe |
| peripheral -> central message | PTX data packet |
| central -> peripheral message | ACK payload where possible; explicit downlink packet later if needed |
| duplicate suppression | `SequenceTracker<MAX_DEVICES>` in RMK adapter |
| binding validation | `StaticBindingTable<PIPE_COUNT>` |

Do not include dynamic pairing, channel hopping, or encryption in the first
RMK ESB prototype.

## Peripheral-Side Adapter Sketch

Peripheral side owns an `EsbPtx`.

Write path:

1. Serialize `SplitMessage` into a stack/static `[u8; SPLIT_MESSAGE_MAX_SIZE]`.
2. `encode_frame(device_id, sequence, flags, serialized, tx_buf)`.
3. `EsbPtx::send_to(pipe, frame).await`.
4. Check `max_attempts_reached()` for diagnostics/retry policy.
5. Consume ACK payloads via `receive()` when central sends downlink messages.

Read path:

1. Wait for ACK payloads or future explicit downlink packets.
2. `decode_frame()`.
3. Check destination/device semantics.
4. Deserialize `SplitMessage`.

## Central-Side Adapter Sketch

Central side owns an `EsbPrx`.

Read path:

1. `EsbPrx::receive().await`.
2. Get `pipe()` from `ReceivedPacket`.
3. Use `transport::accept_bound_frame()` to decode, verify static binding, and
   drop duplicate `device_id + sequence` frames before deserializing.
4. Deserialize `SplitMessage`.

Write path:

1. Serialize central-to-peripheral `SplitMessage`.
2. `encode_frame(device_id, sequence, flags, serialized, tx_buf)`.
3. Queue with `EsbPrx::send_ack_payload(pipe, frame).await`.
4. If ACK payload capacity or timing is not enough, add an explicit downlink
   scheduling policy in RMK instead of hiding it in the ESB core.

## Reliability Rules

- ESB PID/CRC duplicate detection is radio-level only.
- RMK adapter must deduplicate with `device_id + sequence_number`.
- A retransmitted key report must not publish a duplicate key event.
- Static binding mismatch must drop the frame and increment a diagnostic counter.
- `MaxRetransmit` should not imply disconnection immediately; RMK should apply a
  retry/timeout policy based on keyboard latency requirements.

## First Hardware Acceptance

| Scenario | Pass criteria |
|----------|---------------|
| One peripheral | Key press/release arrives once; no duplicate after retransmit. |
| Two peripherals | Both pipes route correctly; no cross-device events. |
| Dongle PRX | Exclusive PRX handles multiple PTX devices without BLE. |
| Main-half PRX + BLE | BLE remains connected while split traffic stays within latency target. |
| Power cycle peripheral | Static binding resumes without central reset. |

Metrics to log:

- `device_id`
- ESB pipe
- sequence number
- duplicate drops
- max-attempt events
- split message latency
- decode/deserialize errors
- binding mismatch drops

## RMK-Side Implementation Checklist

Target file layout in RMK:

| File | Purpose |
|------|---------|
| `rmk/src/split/esb.rs` | ESB implementation of `SplitReader` / `SplitWriter`, mirroring `gazell.rs` structure where useful. |
| `rmk/src/split/mod.rs` | Feature-gate and module export for the ESB split transport. |
| `rmk/Cargo.toml` | Optional dependency on `embassy-nrf-esb` and feature wiring. |
| board/example config | Static binding table, ESB addresses, channel, payload length, and pipe assignment. |

First implementation steps:

1. Add an RMK feature such as `wireless_esb`.
2. Add `split/esb.rs` with a central PRX driver and peripheral PTX driver.
3. Keep serialization identical to existing Gazell/BLE paths: postcard
   `SplitMessage` into `SPLIT_MESSAGE_MAX_SIZE` buffers.
4. Wrap serialized bytes with `embassy_nrf_esb::transport::encode_frame`.
5. On central receive, validate `pipe -> device_id` static binding before
   deserializing.
6. Use `SequenceTracker` before publishing key events.
7. Expose counters for max attempts, duplicate drops, binding mismatch, decode
   errors, and deserialization errors.
8. Start with fixed channel and static addresses.

Do not pull `SplitMessage` or RMK traits into `embassy-nrf-esb`; they are
currently RMK crate-private and should remain owned by RMK.
