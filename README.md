# embassy-nrf-esb

Pure Rust ESB (Enhanced ShockBurst) implementation for nRF52 series, built on Embassy async.

## Status

**Core driver complete.** PTX/PRX state machines, ISR glue, async API, and suspend/resume are implemented and code-reviewed. Hardware verification in progress.

## Features

- PTX (Primary Transmitter) and PRX (Primary Receiver) roles
- ACK with payload (bidirectional data)
- Multi-pipe support (up to 8 pipes)
- Retransmission with configurable attempts and delay
- Embassy async API (`send().await`, `receive().await`)
- Suspend/resume for MPSL timeslots and BLE/ESB hot-switching
- Configurable timer (TIMER1/2/3/4; TIMER0 reserved for MPSL)
- Dynamic payload length (1--252 bytes)
- `defmt` logging (optional feature)

## Quick Start

```rust,ignore
use embassy_nrf_esb::isr::{EsbPtx, DEFAULT_POOL_N, DEFAULT_POOL_SIZE};
use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::payload::PacketPool;

// Create a static packet pool
static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();

// Initialize PTX driver (in async main)
let config = EsbConfig::default();
let addresses = EsbAddresses::default();
let ptx = EsbPtx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses, 0);

// Send a packet
ptx.send(b"hello").await.unwrap();
```

Wire up the interrupt handlers:

```rust,ignore
#[embassy_nrf::pac::interrupt]
fn RADIO() { ptx.on_radio_interrupt(); }

#[embassy_nrf::pac::interrupt]
fn TIMER1() { ptx.on_timer_interrupt(); }
```

See [`examples/`](examples/) for complete PTX, PRX, and USB CDC examples.

## Supported Chips

| Chip | Feature | Status |
|------|---------|--------|
| nRF52840 | `nrf52840` | Primary target |
| nRF52833 | `nrf52833` | Compiles, untested |
| nRF52832 | `nrf52832` | Compiles, untested |

## Cargo Features

| Feature | Description |
|---------|-------------|
| `nrf52840` / `nrf52833` / `nrf52832` | Chip selection (exactly one required) |
| `defmt` | Enable `defmt` logging and `Format` derives |
| `fast-ru` | Enable fast radio ramp-up (40 us instead of 140 us) |
| `board-dongle` / `board-nicenano` | Board-specific memory layout for bootloader |

## Architecture

```
addresses ─┐
config ────┤
header ────┤
           ├── radio ── state_machine ── isr (EsbPtx / EsbPrx)
timer ─────┤                └── suspend
payload ───┘
```

All state machine logic runs in a single ISR context (RADIO ISR). The TIMER ISR is minimal: it sets a flag and pends the RADIO ISR. The async API communicates with the ISR via lock-free packet pool and `embassy-sync` channels.

## Dependencies

- `embassy-nrf` 0.10 (with `unstable-pac`)
- `embassy-sync` 0.8
- `cortex-m` 0.7

## References

- [esb-ng](https://github.com/jamesmunns/esb) -- Reference ESB state machine (Rust)
- [Nordic ESB User Guide](https://infocenter.nordicsemi.com/topic/com.nordic.infocenter.sdk5.v15.0.0/esb_user_guide.html)
- [nRF52840 Product Specification](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/keydoc_html.html)

## Documentation

- [Implementation Plan](docs/plan.md)
- [Hardware Verification Plan](docs/m9-verification.md)
- API docs: `cargo doc --features nrf52840 --open`

## License

MIT OR Apache-2.0
