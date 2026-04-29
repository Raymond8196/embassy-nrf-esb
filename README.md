# embassy-nrf-esb

Pure Rust ESB (Enhanced ShockBurst) implementation for nRF52 series, built on Embassy async.

## Status

**M0 — Repository skeleton.** Core types and build infrastructure in place.

## Goals

1. **Drop-in replacement for Nordic Gazell** in the RMK keyboard firmware
2. **Open-source friendly** — Clean API following Embassy conventions, no PAC types in public API
3. **MPSL timeslot support** — BLE + ESB concurrent operation (Phase 7)

## Features

- PTX (Primary Transmitter) and PRX (Primary Receiver) roles
- ACK with payload (bidirectional data)
- Multi-pipe support (up to 8 pipes)
- Retransmission with configurable attempts
- Embassy async API (`send().await`, `receive().await`)
- Suspend/resume for MPSL timeslots and BLE/ESB hot-switching
- Configurable timer (TIMER1/2/3/4; TIMER0 reserved for MPSL)
- `defmt` support

## Supported Chips

| Chip | Feature | Status |
|------|---------|--------|
| nRF52840 | `nrf52840` | Primary target |
| nRF52833 | `nrf52833` | Reserved |
| nRF52832 | `nrf52832` | Reserved |

## Dependencies

- `embassy-nrf` 0.10 (with `unstable-pac` for PAC access)
- `embassy-sync` 0.8
- `cortex-m` 0.7

Version-aligned with RMK for seamless integration.

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

## License

MIT OR Apache-2.0
