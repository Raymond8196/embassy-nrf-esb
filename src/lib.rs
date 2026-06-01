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
compile_error!("One chip feature must be enabled: nrf52840, nrf52833, or nrf52832");

#[cfg(all(feature = "mpsl", feature = "_cs-cortex"))]
compile_error!("features `mpsl` and `_cs-cortex` are mutually exclusive");

pub mod addresses;
pub mod config;
pub mod error;
pub mod header;
#[cfg(any(test, feature = "mpsl"))]
pub(crate) mod mpsl_common;
#[cfg(feature = "mpsl")]
pub mod mpsl_profile;
#[cfg(feature = "mpsl")]
pub mod mpsl_radio;
pub mod payload;
pub(crate) mod radio;
pub mod state_machine;
pub mod suspend;
pub mod timer;
pub mod transport;

#[cfg(feature = "mpsl")]
pub mod mpsl_timeslot;

// Re-export driver types from isr module
pub mod isr;

/// Define RADIO and TIMER interrupt handlers for an ESB driver reference.
///
/// The first argument is a `static mut Option<&'static EsbPtx<_>>` or
/// `static mut Option<&'static EsbPrx<_>>`. The second argument is the timer
/// interrupt name, for example `TIMER1`.
///
/// This macro is intended for `cortex-m-rt` style examples. Applications using
/// a different interrupt binding model can call `on_radio_interrupt()` and
/// `on_timer_interrupt()` directly from their own handlers.
#[macro_export]
macro_rules! esb_interrupts {
    ($driver_ref:ident, $timer_interrupt:ident $(,)?) => {
        #[cortex_m_rt::interrupt]
        fn RADIO() {
            if let Some(driver) = unsafe { $driver_ref } {
                driver.on_radio_interrupt();
            }
        }

        #[cortex_m_rt::interrupt]
        fn $timer_interrupt() {
            if let Some(driver) = unsafe { $driver_ref } {
                driver.on_timer_interrupt();
            }
        }
    };
}

/// PTX-specific alias for [`esb_interrupts!`].
#[macro_export]
macro_rules! esb_ptx_interrupts {
    ($driver_ref:ident, $timer_interrupt:ident $(,)?) => {
        $crate::esb_interrupts!($driver_ref, $timer_interrupt);
    };
}

/// PRX-specific alias for [`esb_interrupts!`].
#[macro_export]
macro_rules! esb_prx_interrupts {
    ($driver_ref:ident, $timer_interrupt:ident $(,)?) => {
        $crate::esb_interrupts!($driver_ref, $timer_interrupt);
    };
}

pub(crate) use embassy_nrf::pac;
