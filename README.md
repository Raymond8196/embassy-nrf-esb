# embassy-nrf-esb

Pure Rust ESB (Enhanced ShockBurst) implementation for nRF52 series, built on Embassy async.

## Status

**Pre-implementation** — Architecture plan finalized, implementation starting.

## Goals

1. **Self-use in RMK** — Drop-in replacement for Nordic Gazell in the RMK keyboard firmware
2. **Open-source friendly** — Clean API boundaries following Embassy conventions, no PAC types in public API
3. **MPSL timeslot support** — BLE + ESB concurrent operation is a hard requirement

## Features (planned)

- PTX (Primary Transmitter) and PRX (Primary Receiver) roles
- ACK with payload (bidirectional data)
- Multi-pipe support (up to 8 pipes)
- Retransmission with configurable attempts
- Embassy async API (`send().await`, `receive().await`)
- Suspend/resume for MPSL timeslots and BLE/ESB hot-switching
- Configurable timer (TIMER1/2/3/4, TIMER0 reserved for MPSL)
- `defmt` support

## Architecture

```
payload ──────────────────────────────────┐
radio ──── state_machine ──── isr ──── async_driver
timer ──── ┘               └── suspend ──┤
buffer ───────────────────────────────────┘
                                         └── mpsl_timeslot
```

## References

- [esb-ng](https://github.com/jamesmunns/esb) — Reference ESB state machine (Rust, nrf-pac 0.1)
- [Nordic ESB User Guide](https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html)
- [nRF52840 Product Specification](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/keydoc_html.html) — RADIO peripheral (Ch 6.17), TIMER (Ch 6.24)
- [MPSL Timeslot API](https://docs.nordicsemi.com/bundle/ncs-latest/page/nrfxlib/mpsl/doc/timeslot.html)

## Documentation

- [Implementation Plan](docs/plan.md) (English)
- [Implementation Plan](docs/plan.zh.md) (Chinese)

## License

MIT or Apache-2.0 (TBD)
