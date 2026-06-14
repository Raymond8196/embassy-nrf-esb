# embassy-nrf-esb

Pure Rust ESB (Enhanced ShockBurst) implementation for nRF52 series, built on Embassy async.

## Status

**Alpha — core ESB and MPSL BLE+ESB coexistence working, API not yet stable.**

- PTX/PRX ESB with ACK payloads, multi-pipe, NoAck, retransmission, suspend/resume — all hardware verified
- MPSL timeslot integration with Nordic EXTEND mode — hardware verified, 120s stress tested
- BLE + ESB concurrent operation — verified: 100% OK rate with active BLE connection, 274 acked/s throughput, 2.8ms p50 latency
- 15 coexistence profiles for different BLE/ESB duty-cycle tradeoffs
- Real hardware validated on Elytra nRF52833 split keyboard (matrix scan + event-driven ESB PTX + BLE)
- Split transport layer (framing, static binding, sequence dedup) with host tests and a runnable split example pair
- On-target register HIL tests via `cargo xtask test-hw` (embedded-test, runs on nRF52833 over SWD)
- RMK split transport adapter — planned next; the RMK-side `SplitReader`/`SplitWriter` adapter is not written yet

### Performance (NordicExtend profile, 200 evt/s, BLE connected, 120s)

| Metric | Value |
|--------|-------|
| OK rate | 100% (0 drops) |
| Throughput | 274 acked/s |
| Latency p50 / p99 | 2.8ms / 7.6ms |
| TX first-attempt success | 96.6% |
| Extend yield | 99.5% |
| BLE impact on ESB | Zero (OK rate unchanged before/during/after BLE connect) |

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
- Small transport framing helpers for RMK-style multi-split payloads
- Configurable timer (TIMER1/2/3/4; TIMER0 reserved for MPSL)
- `defmt` support

## MPSL Coexistence

The `mpsl` feature enables concurrent BLE and ESB operation via Nordic's
Multi-Protocol Service Layer timeslot API. This is the only Rust implementation
of BLE + proprietary radio coexistence for nRF52.

**Architecture**: PRX requests short initial timeslots (1500us) and dynamically
extends them in 530us increments via MPSL `ACTION_EXTEND`. BLE preemption only
costs one 530us window instead of the entire slot, keeping ESB link alive under
BLE load. This is the Nordic-recommended extend pattern.

The public timeslot API (`open_prx_session`, `open_ptx_session`,
`open_event_session`, `SignalCounters`, profile enums) is functional but not
yet stabilized. The `mpsl` feature is mutually exclusive with `_cs-cortex`.

### 3-Mode Example

`mpsl_3mode_central` + `mpsl_3mode_event` demonstrate simultaneous:
1. ESB PRX/PTX (split keyboard data)
2. BLE connectable peripheral (HID to host)
3. USB CDC (debug statistics)

All three run concurrently on a single nRF52840 with zero ESB degradation.

## Supported Chips

| Chip | Feature | Status |
|------|---------|--------|
| nRF52840 | `nrf52840` | Primary target; current hardware validation target |
| nRF52833 | `nrf52833` | Validated on Elytra split keyboard |
| nRF52832 | `nrf52832` | Feature reserved; not hardware-validated yet |

## Quick Start

Run the host and feature checks:

```bash
cargo fmt --check
cargo test --lib --target x86_64-unknown-linux-gnu --features nrf52840
cargo check --features nrf52840,_cs-cortex
```

### Tests

The `xtask` runner drives both host and on-target tests:

```bash
cargo xtask test-host   # host unit tests (ELF target; matches CI)
cargo xtask test-hw     # on-target register HIL tests (needs a probe on nRF52833)
cargo xtask test        # both
```

`test-hw` flashes the `tests/hw.rs` register-assert suite to an nRF52833 over
SWD via probe-rs and checks that the public driver programs RADIO/TIMER exactly
as configured. The two nRF52840 dongles are DFU-only and cannot run these.

Build the exclusive ESB USB smoke-test firmware for two nRF52840 boards:

```bash
make prx_usb_dfu.zip
make ptx_ack_echo_dfu.zip
```

Flash each board while it is in serial DFU mode:

```bash
make flash-prx_usb PORT=/dev/ttyACM0
make flash-ptx_ack_echo PORT=/dev/ttyACM1
```

Expected smoke-test output is documented in
[Alpha Testing Guide](docs/alpha-testing.md).

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
                                          └── mpsl_timeslot ── mpsl_profile
                                                              mpsl_schedule
```

## References

- [esb-ng](https://github.com/jamesmunns/esb) — Reference ESB state machine (Rust)
- [Nordic ESB User Guide](https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html)
- [nRF52840 Product Specification](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/keydoc_html.html)

## Documentation

- [Implementation Plan](docs/plan.md)
- [Roadmap to 9/10](docs/roadmap-to-9.md)
- [Core ESB Verification](docs/core-verification.md)
- [Alpha Testing Guide](docs/alpha-testing.md)
- [RMK ESB Integration Notes](docs/rmk-integration.md)
- [M10 MPSL Verification](docs/m10-verification.md)
- [ESB+BLE Coexistence Analysis](docs/esb-ble-coexistence-analysis.md)

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

Publishing note: the optional `mpsl` feature currently depends on a pinned git
revision of `nrf-mpsl` for `embassy-nrf` 0.10 compatibility. This is acceptable
for GitHub alpha testing, but it blocks normal crates.io packaging until a
compatible crates.io dependency is available or the MPSL integration is split
from the publishable core.

### Trademarks

"Enhanced ShockBurst" and "ESB" are trademarks of Nordic Semiconductor ASA.
This library is an independent implementation compatible with Nordic's ESB
radio protocol and is not affiliated with or endorsed by Nordic Semiconductor
ASA.
