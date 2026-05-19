//! Pure Rust ESB (Enhanced ShockBurst) for nRF52, built on Embassy async.
//!
//! # Features
//!
//! - PTX (Primary Transmitter) and PRX (Primary Receiver) roles
//! - ACK with payload (bidirectional data)
//! - Multi-pipe support (up to 8 pipes)
//! - Retransmission with configurable attempts
//! - Embassy async API (`send().await`, `receive().await`)
//! - Suspend/resume for MPSL timeslots and BLE/ESB hot-switching
//!
//! # Chip selection
//!
//! One chip feature must be enabled:
//! - `nrf52840` (default target)
//! - `nrf52833`
//! - `nrf52832`
//!
//! # Timer selection
//!
//! Timer peripheral is selected via generics at init time, not feature gates.
//! TIMER0 is reserved for MPSL. Use TIMER1–TIMER4.

#![no_std]

// Chip feature guard. Host-side unit tests intentionally compile without a chip.
#[cfg(all(
    not(test),
    not(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))
))]
compile_error!(
    "One chip feature must be enabled: nrf52840, nrf52833, or nrf52832"
);

#[cfg(all(feature = "mpsl", feature = "_cs-cortex"))]
compile_error!("features `mpsl` and `_cs-cortex` are mutually exclusive");

pub mod addresses;
pub mod config;
pub mod error;
pub mod header;
pub mod payload;
pub(crate) mod radio;
pub mod state_machine;
pub mod suspend;
pub mod timer;

#[cfg(feature = "mpsl")]
pub mod mpsl_timeslot;

// Re-export driver types from isr module
pub mod isr;

// Re-export PAC for internal use and advanced users
pub use embassy_nrf::pac;
