# Alpha Testing Guide

This guide is for early GitHub testers who want to build and smoke-test the
exclusive ESB path on nRF52840 hardware. The MPSL examples are useful for
diagnostics, but they are not a stable public API yet.

## Current Scope

Expected to work:

- Exclusive PTX/PRX examples on nRF52840.
- ACK payloads.
- NoAck sends.
- Multi-pipe routing.
- Suspend/resume smoke tests.

Not ready as a stable user-facing feature:

- RMK adapter integration.
- Dynamic pairing.
- Channel hopping.
- Encryption.
- Product-ready BLE + ESB coexistence through MPSL.
- crates.io publishing with the optional `mpsl` feature.

## Payload Length

`EsbConfig::default()` uses a 32-byte maximum payload for ESB-compatible smoke
tests. Higher-level transports that wrap application payloads in an additional
header should set a larger value explicitly:

```rust
let config = EsbConfig::default().with_payload_length(required_payload_len);
```

For frames built with this crate's `transport` module, calculate the required
ESB payload length with `transport::required_esb_payload_len(app_payload_len)`
and validate it with `transport::validate_payload_length(config.payload_length,
app_payload_len)`.

## Build Gate

Run this before reporting hardware results:

```bash
cargo fmt --check
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
cargo check --features nrf52840,_cs-cortex
cargo check --example ptx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_basic --features nrf52840,defmt,_cs-cortex
cargo check --example prx_usb --features nrf52840,_cs-cortex
cargo check --example ptx_silent --features nrf52840,_cs-cortex
cargo check --example usb_minimal --features nrf52840,_cs-cortex
cargo check --example ptx_ack_echo --features nrf52840,_cs-cortex
cargo check --example ptx_multipipe --features nrf52840,defmt,_cs-cortex
cargo check --example prx_multipipe_usb --features nrf52840,_cs-cortex
cargo check --example ptx_multipipe_ack --features nrf52840,_cs-cortex
cargo check --example ptx_suspend --features nrf52840,defmt,_cs-cortex
cargo check --example ptx_noack_usb --features nrf52840,_cs-cortex
cargo check --example prx_noack_usb --features nrf52840,_cs-cortex
cargo check --example ptx_suspend_usb --features nrf52840,_cs-cortex
cargo check --example prx_idle_usb --features nrf52840,_cs-cortex
```

The intentional feature-conflict check should fail with a clear mutual
exclusion error:

```bash
cargo check --features nrf52840,mpsl,_cs-cortex
```

## Two-Dongle Smoke Test

The fastest useful hardware check is `prx_usb` plus `ptx_ack_echo`.

Build DFU packages:

```bash
make prx_usb_dfu.zip
make ptx_ack_echo_dfu.zip
```

Flash each package with your board's serial DFU port:

```bash
make flash-prx_usb PORT=/dev/ttyACM0
make flash-ptx_ack_echo PORT=/dev/ttyACM1
```

Capture the USB CDC output from both boards for at least 60 seconds. A useful
report includes:

- Commit hash.
- Board model.
- Example pair.
- Cargo features.
- RF channel and payload length if changed from defaults.
- PTX `tx`, `ack_rx`, and max-attempt counters.
- PRX `rx`, `lost`, and loss percentage.
- Any panic, reset loop, or stalled counter.

## Known Packaging Limitation

The optional `mpsl` feature currently depends on a pinned git revision of
`nrf-mpsl` because the crates.io release has not caught up with the
`embassy-nrf` version used here. This is fine for GitHub-based alpha testing,
but it blocks a normal crates.io package until that dependency can be expressed
with a compatible crates.io version or the MPSL integration is split from the
publishable core.
