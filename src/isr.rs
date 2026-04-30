//! ESB ISR glue and async driver API.
//!
//! Provides the user-facing API for ESB PTX and PRX modes:
//! - `EsbPtx`: async PTX driver with `send().await`
//! - `EsbPrx`: async PRX driver with `receive().await`
//! - ISR handler methods called from `#[interrupt]` handlers
//!
//! # Single-ISR-context architecture (R9)
//!
//! All ESB state machine logic runs in the RADIO ISR. The TIMER ISR is
//! minimal: clear events, set flag, pend RADIO ISR.
//!
//! # Usage
//!
//! ```ignore
//! static ESB: StaticCell<EsbPtx<TIMER1>> = StaticCell::new();
//! let ptx = ESB.init(EsbPtx::new(timer, radio, &pool, &config, &addresses, 0));
//!
//! #[embassy_nrf::pac::interrupt]
//! fn RADIO() { ptx.on_radio_interrupt(); }
//!
//! #[embassy_nrf::pac::interrupt]
//! fn TIMER1() { ptx.on_timer_interrupt(); }
//! ```

use core::sync::atomic::{AtomicBool, Ordering};

use crate::addresses::EsbAddresses;
use crate::config::EsbConfig;
use crate::error::Error;
use crate::header::EsbHeader;
use crate::payload::PacketPool;
use crate::radio::EsbRadio;
use crate::state_machine::{PtxStateMachine, PrxStateMachine};
use crate::timer::{EsbTimer, TimerInstance};

/// Default pool size (number of packet buffers).
pub const DEFAULT_POOL_N: usize = 4;

/// Default buffer size per packet (4-byte header + 252-byte payload).
pub const DEFAULT_POOL_SIZE: usize = 256;

// ---- PTX Driver ----

/// Embassy async PTX (Primary Transmitter) driver.
///
/// Combines radio, timer, packet pool, and PTX state machine.
/// Place in a `static` via `static_cell`.
pub struct EsbPtx<T: TimerInstance, const N: usize = DEFAULT_POOL_N, const SIZE: usize = DEFAULT_POOL_SIZE> {
    sm: PtxStateMachine<T>,
    pool: &'static PacketPool<N, SIZE>,
    /// Shared flag set by TIMER ISR, read/cleared by RADIO ISR.
    timer_flag: AtomicBool,
}

// SAFETY: Placed in static by user. All ISR access is single-threaded.
// pool pointer is valid for 'static.
unsafe impl<T: TimerInstance, const N: usize, const SIZE: usize> Send for EsbPtx<T, N, SIZE> {}
unsafe impl<T: TimerInstance, const N: usize, const SIZE: usize> Sync for EsbPtx<T, N, SIZE> {}

#[allow(dead_code)]
impl<T: TimerInstance, const N: usize, const SIZE: usize> EsbPtx<T, N, SIZE> {
    /// Create a new PTX driver.
    ///
    /// - `timer`: Embassy TIMER peripheral (consumed, prevents other access)
    /// - `_radio`: Embassy RADIO peripheral (consumed for ownership proof)
    /// - `pool`: Static packet pool
    /// - `config`: ESB configuration
    /// - `addresses`: ESB address configuration
    /// - `tx_pipe`: Pipe number for TX (typically 0)
    pub fn new(
        timer: embassy_nrf::Peri<'static, T>,
        _radio: embassy_nrf::Peri<'static, embassy_nrf::peripherals::RADIO>,
        pool: &'static PacketPool<N, SIZE>,
        config: &EsbConfig,
        addresses: &EsbAddresses,
        tx_pipe: u8,
    ) -> Self {
        let mut radio = EsbRadio::new(crate::pac::RADIO);
        radio.init(config, addresses);

        let esb_timer = EsbTimer::new(timer);
        let sm = PtxStateMachine::new(radio, esb_timer, config, tx_pipe);

        Self {
            sm,
            pool,
            timer_flag: AtomicBool::new(false),
        }
    }

    /// Called from the RADIO interrupt handler.
    ///
    /// Processes radio events and advances the PTX state machine.
    pub fn on_radio_interrupt(&mut self) {
        let timer_flag = self.timer_flag.load(Ordering::Acquire);
        if timer_flag {
            self.timer_flag.store(false, Ordering::Release);
        }
        self.sm.handle_radio_event(self.pool, timer_flag);
    }

    /// Called from the TIMER interrupt handler.
    ///
    /// Minimal: clears timer events, sets flag, pends RADIO ISR (R9).
    pub fn on_timer_interrupt(&self) {
        self.sm.handle_timer_event();
        self.timer_flag.store(true, Ordering::Release);
    }

    /// Queue a packet for transmission.
    ///
    /// Returns after queuing. The packet will be sent in the next
    /// RADIO ISR cycle.
    pub async fn send(&self, payload: &[u8]) -> Result<(), Error> {
        let idx = self.pool.alloc_tx().ok_or(Error::TxFull)?;

        // Fill the buffer
        let header = unsafe { self.pool.header_mut(idx) };
        header.length = payload.len() as u8;
        header.set_no_ack(false);

        let buf = unsafe { self.pool.buf_mut(idx) };
        let payload_offset = EsbHeader::DMA_OFFSET + 2; // after length + pid_no_ack
        if payload.len() <= 252 && payload_offset + payload.len() <= buf.len() {
            buf[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);
        }

        self.pool.enqueue_tx(idx).await;
        self.sm.trigger_send();
        Ok(())
    }

    /// Queue a NoAck packet for transmission.
    ///
    /// NoAck packets do not wait for acknowledgment — fire and forget.
    pub async fn send_no_ack(&self, payload: &[u8]) -> Result<(), Error> {
        let idx = self.pool.alloc_tx().ok_or(Error::TxFull)?;

        let header = unsafe { self.pool.header_mut(idx) };
        header.length = payload.len() as u8;
        header.set_no_ack(true);

        let buf = unsafe { self.pool.buf_mut(idx) };
        let payload_offset = EsbHeader::DMA_OFFSET + 2;
        if payload.len() <= 252 && payload_offset + payload.len() <= buf.len() {
            buf[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);
        }

        self.pool.enqueue_tx(idx).await;
        self.sm.trigger_send();
        Ok(())
    }

    /// Try to receive an ACK payload (non-blocking).
    pub fn try_receive(&self) -> Option<ReceivedPacket<'_, N, SIZE>> {
        let idx = self.pool.try_receive_rx()?;
        Some(ReceivedPacket {
            pool: self.pool,
            idx,
        })
    }

    /// Receive an ACK payload (async).
    pub async fn receive(&self) -> ReceivedPacket<'_, N, SIZE> {
        let idx = self.pool.receive_rx().await;
        ReceivedPacket {
            pool: self.pool,
            idx,
        }
    }

    /// Get current PTX state.
    pub fn state(&self) -> crate::state_machine::StatePtx {
        self.sm.state()
    }
}

// ---- PRX Driver ----

/// Embassy async PRX (Primary Receiver) driver.
pub struct EsbPrx<T: TimerInstance, const N: usize = DEFAULT_POOL_N, const SIZE: usize = DEFAULT_POOL_SIZE> {
    sm: PrxStateMachine<T>,
    pool: &'static PacketPool<N, SIZE>,
    timer_flag: AtomicBool,
}

unsafe impl<T: TimerInstance, const N: usize, const SIZE: usize> Send for EsbPrx<T, N, SIZE> {}
unsafe impl<T: TimerInstance, const N: usize, const SIZE: usize> Sync for EsbPrx<T, N, SIZE> {}

#[allow(dead_code)]
impl<T: TimerInstance, const N: usize, const SIZE: usize> EsbPrx<T, N, SIZE> {
    /// Create a new PRX driver.
    pub fn new(
        timer: embassy_nrf::Peri<'static, T>,
        _radio: embassy_nrf::Peri<'static, embassy_nrf::peripherals::RADIO>,
        pool: &'static PacketPool<N, SIZE>,
        config: &EsbConfig,
        addresses: &EsbAddresses,
    ) -> Self {
        let mut radio = EsbRadio::new(crate::pac::RADIO);
        radio.init(config, addresses);

        let esb_timer = EsbTimer::new(timer);
        let enabled_pipes = addresses.enabled_mask();
        let sm = PrxStateMachine::new(radio, esb_timer, config, enabled_pipes);

        Self {
            sm,
            pool,
            timer_flag: AtomicBool::new(false),
        }
    }

    /// Called from the RADIO interrupt handler.
    pub fn on_radio_interrupt(&mut self) {
        let timer_flag = self.timer_flag.load(Ordering::Acquire);
        if timer_flag {
            self.timer_flag.store(false, Ordering::Release);
        }
        self.sm.handle_radio_event(self.pool, timer_flag);
    }

    /// Called from the TIMER interrupt handler.
    pub fn on_timer_interrupt(&self) {
        self.sm.handle_timer_event();
    }

    /// Start listening for incoming packets.
    pub fn start_listening(&mut self) -> Result<(), Error> {
        self.sm.start_receiving(self.pool)
    }

    /// Receive the next packet (async).
    pub async fn receive(&self) -> ReceivedPacket<'_, N, SIZE> {
        let idx = self.pool.receive_rx().await;
        ReceivedPacket {
            pool: self.pool,
            idx,
        }
    }

    /// Queue an ACK payload for a specific pipe.
    pub async fn send_ack_payload(&self, pipe: u8, payload: &[u8]) -> Result<(), Error> {
        let idx = self.pool.alloc_tx().ok_or(Error::TxFull)?;

        let header = unsafe { self.pool.header_mut(idx) };
        header.length = payload.len() as u8;
        header.pipe = pipe;
        header.set_no_ack(false);

        let buf = unsafe { self.pool.buf_mut(idx) };
        let payload_offset = EsbHeader::DMA_OFFSET + 2;
        if payload.len() <= 252 && payload_offset + payload.len() <= buf.len() {
            buf[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);
        }

        self.pool.enqueue_tx(idx).await;
        Ok(())
    }

    /// Stop listening and go idle.
    pub fn stop(&mut self) {
        self.sm.stop_receiving(self.pool);
    }

    /// Get current PRX state.
    pub fn state(&self) -> crate::state_machine::StatePrx {
        self.sm.state()
    }
}

// ---- Received Packet Handle ----

/// A received packet from the ESB pool.
///
/// Provides read access to the packet metadata and payload.
/// Drop to release the buffer back to the pool.
pub struct ReceivedPacket<'a, const N: usize, const SIZE: usize> {
    pool: &'a PacketPool<N, SIZE>,
    idx: usize,
}

#[allow(dead_code)]
impl<const N: usize, const SIZE: usize> ReceivedPacket<'_, N, SIZE> {
    /// Get the pipe number.
    pub fn pipe(&self) -> u8 {
        let header = unsafe { self.pool.header_mut(self.idx) };
        header.pipe
    }

    /// Get the RSSI value.
    pub fn rssi(&self) -> u8 {
        let header = unsafe { self.pool.header_mut(self.idx) };
        header.rssi
    }

    /// Get the payload length.
    pub fn len(&self) -> usize {
        let header = unsafe { self.pool.header_mut(self.idx) };
        header.length as usize
    }

    /// Check if the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the payload as a byte slice.
    pub fn payload(&self) -> &[u8] {
        let buf = unsafe { self.pool.buf(self.idx) };
        let header = unsafe { self.pool.header_mut(self.idx) };
        let len = header.length as usize;
        let payload_offset = EsbHeader::DMA_OFFSET + 2;
        let end = (payload_offset + len).min(buf.len());
        &buf[payload_offset..end]
    }

    /// Get the pool index (for advanced use).
    pub fn index(&self) -> usize {
        self.idx
    }
}

impl<const N: usize, const SIZE: usize> Drop for ReceivedPacket<'_, N, SIZE> {
    fn drop(&mut self) {
        self.pool.release_rx(self.idx);
    }
}
