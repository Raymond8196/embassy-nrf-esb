# embassy-nrf-esb

Pure Rust ESB (Enhanced ShockBurst) implementation for nRF52 series, built on Embassy async.

## Status

**Active development — ESB core and MPSL bring-up.**

- Exclusive PTX/PRX ESB examples build and have been hardware-smoke tested during M9/M10 work.
- ACK payloads, multi-pipe routing, NoAck sends, retransmission, and suspend/resume are implemented.
- MPSL timeslot diagnostics for PTX/PRX build and have shown two-dongle multi-pipe ACK payload success.
- BLE + ESB coexistence is functional in diagnostics, but active BLE connection scheduling still needs tuning before it is treated as product-ready.
- RMK integration is planned next; no RMK transport adapter is included yet.

The current roadmap is tracked in [Roadmap to 9/10](docs/roadmap-to-9.md).

## Goals

1. **Nordic ESB-compatible transport**; can replace Gazell-based RMK links when both sides are migrated
2. **RMK multi-split support** — static-bound multi-pipe peripherals first, pairing/channel hopping later
3. **Open-source friendly** — Clean API following Embassy conventions, no PAC types in the normal public API
4. **MPSL timeslot support** — BLE + ESB concurrent operation behind an optional feature

## Features

- PTX (Primary Transmitter) and PRX (Primary Receiver) roles
- ACK with payload (bidirectional data)
- Multi-pipe support (up to 8 pipes)
- Retransmission with configurable attempts
- Embassy async API (`send().await`, `receive().await`)
- Suspend/resume for MPSL timeslots and BLE/ESB hot-switching
- Configurable timer (TIMER1/2/3/4; TIMER0 reserved for MPSL)
- `defmt` support

## MPSL Status

The `mpsl` feature is experimental. It currently provides diagnostic helpers
and examples for MPSL timeslot bring-up, PTX/PRX in slots, and nRF SDC BLE
coexistence. These examples are useful for hardware validation, but the public
timeslot API is not yet the final owned wrapper described in
`docs/roadmap-to-9.md`.

Do not treat the current MPSL free-function diagnostics as a stable API.

## Supported Chips

| Chip | Feature | Status |
|------|---------|--------|
| nRF52840 | `nrf52840` | Primary target; current hardware validation target |
| nRF52833 | `nrf52833` | Feature reserved; not hardware-validated yet |
| nRF52832 | `nrf52832` | Feature reserved; not hardware-validated yet |

## Dependencies

- `embassy-nrf` 0.10 (with `unstable-pac` for PAC access)
- `embassy-sync` 0.8
- `cortex-m` 0.7

Version-aligned with RMK for seamless integration.

## Compatibility

This crate implements Nordic's ESB radio packet format and is not wire-compatible with Nordic Gazell by itself. Gazell adds host identity, pairing, channel hopping, and scheduling behavior above ESB. Existing Gazell devices need both sides migrated to this crate or to another compatible ESB protocol layer.

## Clock Ownership

The library does not own HFCLK. In exclusive ESB mode, applications must start and hold the external high-frequency clock before using the RADIO. The examples do this explicitly with `CLOCK.tasks_hfclkstart()`.

With the optional `mpsl` feature, applications should use `nrf-mpsl` for clock ownership, for example by holding the guard returned by `MultiprotocolServiceLayer::request_hfclk()`. The `mpsl` feature uses `nrf-mpsl`'s critical-section implementation and is mutually exclusive with the internal `_cs-cortex` example feature.

## Architecture

```
payload ──────────────────────────────────┐
radio ──── state_machine ──── isr ──── async_driver
timer ──── ┘               └── suspend ──┤
buffer ───────────────────────────────────┘
                                         └── mpsl_timeslot
```

## References

- [esb-ng](https://github.com/jamesmunns/esb) — Reference ESB state machine (Rust)
- [Nordic ESB User Guide](https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html)
- [nRF52840 Product Specification](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/keydoc_html.html)

## Documentation

- [Implementation Plan](docs/plan.md)
- [Roadmap to 9/10](docs/roadmap-to-9.md)
- [Core ESB Verification](docs/core-verification.md)
- [M10 MPSL Verification](docs/m10-verification.md)

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE-2.0](LICENSE-APACHE-2.0) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

### Third-party notices

Portions of `src/timer.rs`, `src/radio.rs`, and `src/isr.rs` are adapted from
the [esb-ng](https://github.com/jamesmunns/esb) crate (MIT OR Apache-2.0).
See [NOTICE.md](NOTICE.md) for full attribution.

The optional `mpsl` Cargo feature links against `libmpsl.a`, a pre-compiled
binary library provided by Nordic Semiconductor under the Nordic-5-Clause
license. This restricts use to Nordic Semiconductor integrated circuits
(the nRF52 series). Without the `mpsl` feature, this crate is pure Rust under
MIT OR Apache-2.0.

### Trademarks

"Enhanced ShockBurst" and "ESB" are trademarks of Nordic Semiconductor ASA.
This library is an independent implementation compatible with Nordic's ESB
radio protocol and is not affiliated with or endorsed by Nordic Semiconductor
ASA.
