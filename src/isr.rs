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

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

use crate::addresses::EsbAddresses;
use crate::config::EsbConfig;
use crate::error::Error;
use crate::header::EsbHeader;
use crate::payload::PacketPool;
use crate::radio::EsbRadio;
use crate::state_machine::{PrxStateMachine, PtxStateMachine};
use crate::suspend::EsbSavedState;
use crate::timer::{EsbTimer, TimerInstance};

/// Default pool size (number of packet buffers).
pub const DEFAULT_POOL_N: usize = 4;

/// Default buffer size per packet (4-byte header + 252-byte payload).
pub const DEFAULT_POOL_SIZE: usize = 256;

// ---- NVIC helpers ----

#[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
fn disable_radio_irq() {
    cortex_m::peripheral::NVIC::mask(crate::pac::Interrupt::RADIO);
}

#[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
fn enable_radio_irq() {
    // SAFETY: Caller ensures no data races — called only when the driver
    // owns the radio (after suspend or during restore).
    unsafe {
        cortex_m::peripheral::NVIC::unmask(crate::pac::Interrupt::RADIO);
    }
}

#[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
fn unpend_radio_irq() {
    cortex_m::peripheral::NVIC::unpend(crate::pac::Interrupt::RADIO);
}

// ---- PTX Driver ----

/// Embassy async PTX (Primary Transmitter) driver.
///
/// Combines radio, timer, packet pool, and PTX state machine.
/// Place in a `static` via `static_cell`.
pub struct EsbPtx<
    T: TimerInstance,
    const N: usize = DEFAULT_POOL_N,
    const SIZE: usize = DEFAULT_POOL_SIZE,
> {
    sm: UnsafeCell<PtxStateMachine<T>>,
    pool: &'static PacketPool<N, SIZE>,
    /// Shared flag set by TIMER ISR, read/cleared by RADIO ISR.
    timer_flag: AtomicBool,
    /// Set by RADIO ISR when max retransmit attempts reached, cleared by app.
    max_attempts_flag: AtomicBool,
    /// Set by app to request ISR to stop after current transaction.
    suspend_requested: AtomicBool,
    /// Signaled by ISR when it goes idle with suspend_requested set.
    suspend_signal: Signal<CriticalSectionRawMutex, ()>,
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
        config.validate().expect("ESB config invalid");
        let mut radio = EsbRadio::new(crate::pac::RADIO);
        radio.init(config, addresses);

        let esb_timer = EsbTimer::new(timer);
        let sm = PtxStateMachine::new(radio, esb_timer, config, tx_pipe);

        Self {
            sm: UnsafeCell::new(sm),
            pool,
            timer_flag: AtomicBool::new(false),
            max_attempts_flag: AtomicBool::new(false),
            suspend_requested: AtomicBool::new(false),
            suspend_signal: Signal::new(),
        }
    }

    /// Called from the RADIO interrupt handler.
    ///
    /// Processes radio events and advances the PTX state machine.
    /// Unpends RADIO ISR to prevent spurious re-entry (esb-ng line 149).
    ///
    /// SAFETY: Must be called from RADIO ISR only. No concurrent ISR execution.
    pub fn on_radio_interrupt(&self) {
        let timer_flag = self.timer_flag.load(Ordering::Acquire);
        if timer_flag {
            self.timer_flag.store(false, Ordering::Release);
        }
        let suppress = self.suspend_requested.load(Ordering::Acquire);
        // SAFETY: ISR-only access — no concurrent ISR or app mutation of sm.
        let sm = unsafe { &mut *self.sm.get() };
        let event = sm.handle_radio_event(self.pool, timer_flag, suppress);
        if event == crate::state_machine::PtxEvent::MaxAttempts {
            self.max_attempts_flag.store(true, Ordering::Release);
        }

        if suppress && sm.state() == crate::state_machine::StatePtx::Idle {
            self.suspend_signal.signal(());
        }

        // Clear any latched RADIO pending bit to prevent spurious re-entry
        // (esb-ng irq.rs line 149).
        #[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
        cortex_m::peripheral::NVIC::unpend(crate::pac::Interrupt::RADIO);
    }

    /// Called from the TIMER interrupt handler.
    ///
    /// Minimal: sets flag, then clears timer events and pends RADIO ISR (R9).
    /// Flag must be set BEFORE pending RADIO ISR to prevent race (esb-ng line 63).
    pub fn on_timer_interrupt(&self) {
        self.timer_flag.store(true, Ordering::Release);
        // SAFETY: ISR-only access.
        let sm = unsafe { &*self.sm.get() };
        sm.handle_timer_event();
    }

    /// Queue a packet for transmission.
    ///
    /// Returns after queuing. The packet will be sent in the next
    /// RADIO ISR cycle.
    pub async fn send(&self, payload: &[u8]) -> Result<(), Error> {
        if payload.is_empty() || payload.len() > 252 {
            return Err(Error::InvalidParam);
        }
        let payload_offset = EsbHeader::PAYLOAD_OFFSET;
        if payload_offset + payload.len() > SIZE {
            return Err(Error::InvalidParam);
        }
        let idx = self.pool.alloc_tx().ok_or(Error::TxFull)?;

        let header = unsafe { self.pool.header_mut(idx) };
        header.length = payload.len() as u8;
        header.set_no_ack(false);

        let buf = unsafe { self.pool.buf_mut(idx) };
        buf[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);

        self.pool.enqueue_tx(idx).await;
        // SAFETY: trigger_send only writes NVIC, no data race with ISR.
        unsafe { &*self.sm.get() }.trigger_send();
        Ok(())
    }

    /// Queue a NoAck packet for transmission.
    ///
    /// NoAck packets do not wait for acknowledgment — fire and forget.
    pub async fn send_no_ack(&self, payload: &[u8]) -> Result<(), Error> {
        if payload.is_empty() || payload.len() > 252 {
            return Err(Error::InvalidParam);
        }
        let payload_offset = EsbHeader::PAYLOAD_OFFSET;
        if payload_offset + payload.len() > SIZE {
            return Err(Error::InvalidParam);
        }
        let idx = self.pool.alloc_tx().ok_or(Error::TxFull)?;

        let header = unsafe { self.pool.header_mut(idx) };
        header.length = payload.len() as u8;
        header.set_no_ack(true);

        let buf = unsafe { self.pool.buf_mut(idx) };
        buf[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);

        self.pool.enqueue_tx(idx).await;
        // SAFETY: trigger_send only writes NVIC, no data race with ISR.
        unsafe { &*self.sm.get() }.trigger_send();
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
        // SAFETY: Read-only access, state is updated atomically by ISR.
        unsafe { &*self.sm.get() }.state()
    }

    /// Check if max retransmit attempts was reached since last check.
    ///
    /// Returns `true` if a packet was dropped due to max retransmit.
    /// Clears the flag on read.
    pub fn max_attempts_reached(&self) -> bool {
        self.max_attempts_flag.swap(false, Ordering::AcqRel)
    }

    /// Check if the PTX transmitter is idle (no packet in flight).
    pub fn is_tx_idle(&self) -> bool {
        self.state() == crate::state_machine::StatePtx::Idle
    }

    // ---- Suspend / Resume ----

    /// Non-blocking suspend. Returns `Err(Busy)` if mid-transaction.
    ///
    /// After success, RADIO IRQ is disabled and the radio is stopped.
    /// Call `restore()` to re-initialize and resume operation.
    pub fn try_suspend(&self) -> Result<EsbSavedState, Error> {
        disable_radio_irq();

        let sm = unsafe { &mut *self.sm.get() };
        if sm.state() != crate::state_machine::StatePtx::Idle {
            enable_radio_irq();
            return Err(Error::Busy);
        }

        let saved = sm.do_suspend(self.pool);
        unpend_radio_irq();
        Ok(saved)
    }

    /// Async suspend. Waits for current transaction to complete, then suspends.
    ///
    /// If already idle, returns immediately. Otherwise, signals the ISR to
    /// stop after the current transaction and waits for it to go idle.
    ///
    /// After return, RADIO IRQ is disabled and the radio is stopped.
    /// Call `restore()` to re-initialize and resume operation.
    pub async fn suspend(&self) -> EsbSavedState {
        // Fast path: already idle
        if let Ok(saved) = self.try_suspend() {
            return saved;
        }

        // Set the flag so ISR will suppress new TX after current transaction.
        self.suspend_signal.reset();
        self.suspend_requested.store(true, Ordering::Release);

        // Re-check under IRQ-disabled to close the race window where
        // the ISR completed between try_suspend() fail and the store above.
        disable_radio_irq();
        let sm = unsafe { &mut *self.sm.get() };
        if sm.state() == crate::state_machine::StatePtx::Idle {
            let saved = sm.do_suspend(self.pool);
            unpend_radio_irq();
            self.suspend_requested.store(false, Ordering::Release);
            return saved;
        }
        enable_radio_irq();

        // ISR is running and will see suspend_requested — wait for signal.
        self.suspend_signal.wait().await;

        // ISR is done and state is idle — finalize
        disable_radio_irq();
        let sm = unsafe { &mut *self.sm.get() };
        let saved = sm.do_suspend(self.pool);
        unpend_radio_irq();
        self.suspend_requested.store(false, Ordering::Release);
        saved
    }

    /// Restore ESB from saved state. Full RADIO re-init + PID/CRC restore.
    ///
    /// Must be called after `suspend()` or `try_suspend()`. Re-enables RADIO IRQ.
    /// Does NOT automatically start sending — queued packets will be sent on
    /// the next `send()` call or if packets are already queued in the channel.
    pub fn restore(&self, state: &EsbSavedState, addresses: &EsbAddresses) {
        // SAFETY: RADIO IRQ is disabled (from suspend), so no ISR concurrency.
        let sm = unsafe { &mut *self.sm.get() };
        sm.do_restore(state, addresses);
        self.suspend_requested.store(false, Ordering::Release);
        unpend_radio_irq();
        enable_radio_irq();
    }
}

// ---- PRX Driver ----

/// Embassy async PRX (Primary Receiver) driver.
pub struct EsbPrx<
    T: TimerInstance,
    const N: usize = DEFAULT_POOL_N,
    const SIZE: usize = DEFAULT_POOL_SIZE,
> {
    sm: UnsafeCell<PrxStateMachine<T>>,
    pool: &'static PacketPool<N, SIZE>,
    timer_flag: AtomicBool,
    /// Set by app to request ISR to stop after current ACK TX.
    suspend_requested: AtomicBool,
    /// Signaled by ISR when it returns to Receiver/Idle with suspend_requested set.
    suspend_signal: Signal<CriticalSectionRawMutex, ()>,
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
        config.validate().expect("ESB config invalid");
        let mut radio = EsbRadio::new(crate::pac::RADIO);
        radio.init(config, addresses);

        let esb_timer = EsbTimer::new(timer);
        let enabled_pipes = addresses.enabled_mask();
        let sm = PrxStateMachine::new(radio, esb_timer, config, enabled_pipes);

        Self {
            sm: UnsafeCell::new(sm),
            pool,
            timer_flag: AtomicBool::new(false),
            suspend_requested: AtomicBool::new(false),
            suspend_signal: Signal::new(),
        }
    }

    /// Called from the RADIO interrupt handler.
    ///
    /// SAFETY: Must be called from RADIO ISR only. No concurrent ISR execution.
    pub fn on_radio_interrupt(&self) {
        let timer_flag = self.timer_flag.load(Ordering::Acquire);
        if timer_flag {
            self.timer_flag.store(false, Ordering::Release);
        }
        // SAFETY: ISR-only access.
        let sm = unsafe { &mut *self.sm.get() };
        sm.handle_radio_event(self.pool, timer_flag);

        // If suspend was requested and we're in a suspendable state
        // (Idle or Receiver), signal the waiting async task.
        if self.suspend_requested.load(Ordering::Acquire) {
            let state = sm.state();
            if state == crate::state_machine::StatePrx::Idle
                || state == crate::state_machine::StatePrx::Receiver
            {
                self.suspend_signal.signal(());
            }
        }

        #[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
        cortex_m::peripheral::NVIC::unpend(crate::pac::Interrupt::RADIO);
    }

    /// Called from the TIMER interrupt handler.
    pub fn on_timer_interrupt(&self) {
        self.timer_flag.store(true, Ordering::Release);
        // SAFETY: ISR-only access.
        let sm = unsafe { &*self.sm.get() };
        sm.handle_timer_event();
    }

    /// Start listening for incoming packets.
    pub fn start_listening(&self) -> Result<(), Error> {
        // SAFETY: App context only; ISR won't modify state until we
        // call start_receiving which arms the radio.
        let sm = unsafe { &mut *self.sm.get() };
        sm.start_receiving(self.pool)
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
        if payload.is_empty() || payload.len() > 252 {
            return Err(Error::InvalidParam);
        }
        let payload_offset = EsbHeader::PAYLOAD_OFFSET;
        if payload_offset + payload.len() > SIZE {
            return Err(Error::InvalidParam);
        }
        let idx = self.pool.alloc_tx().ok_or(Error::TxFull)?;

        let header = unsafe { self.pool.header_mut(idx) };
        header.length = payload.len() as u8;
        header.pipe = pipe;
        header.set_no_ack(false);

        let buf = unsafe { self.pool.buf_mut(idx) };
        buf[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);

        self.pool.enqueue_tx(idx).await;
        Ok(())
    }

    /// Stop listening and go idle.
    pub fn stop(&self) {
        // SAFETY: App context; ISR won't fire after we stop the radio.
        let sm = unsafe { &mut *self.sm.get() };
        sm.stop_receiving(self.pool);
    }

    /// Get current PRX state.
    pub fn state(&self) -> crate::state_machine::StatePrx {
        // SAFETY: Read-only access.
        unsafe { &*self.sm.get() }.state()
    }

    // ---- Suspend / Resume ----

    /// Non-blocking suspend. Returns `Err(Busy)` if mid-ACK-TX.
    ///
    /// PRX in `Idle` or `Receiver` state can be suspended immediately.
    /// `TxAck`/`TxRepeatedAck` states return `Err(Busy)`.
    pub fn try_suspend(&self) -> Result<EsbSavedState, Error> {
        disable_radio_irq();

        let sm = unsafe { &mut *self.sm.get() };
        let state = sm.state();
        if state != crate::state_machine::StatePrx::Idle
            && state != crate::state_machine::StatePrx::Receiver
        {
            enable_radio_irq();
            return Err(Error::Busy);
        }

        let saved = sm.do_suspend(self.pool);
        unpend_radio_irq();
        Ok(saved)
    }

    /// Async suspend. Waits for any in-progress ACK TX to complete.
    ///
    /// If in `Idle` or `Receiver` state, returns immediately.
    /// Otherwise waits for the ISR to finish the ACK TX.
    pub async fn suspend(&self) -> EsbSavedState {
        if let Ok(saved) = self.try_suspend() {
            return saved;
        }

        self.suspend_signal.reset();
        self.suspend_requested.store(true, Ordering::Release);

        // Re-check under IRQ-disabled to close the race window where
        // the ISR completed between try_suspend() fail and the store above.
        disable_radio_irq();
        let sm = unsafe { &mut *self.sm.get() };
        let state = sm.state();
        if state == crate::state_machine::StatePrx::Idle
            || state == crate::state_machine::StatePrx::Receiver
        {
            let saved = sm.do_suspend(self.pool);
            unpend_radio_irq();
            self.suspend_requested.store(false, Ordering::Release);
            return saved;
        }
        enable_radio_irq();

        self.suspend_signal.wait().await;

        disable_radio_irq();
        let sm = unsafe { &mut *self.sm.get() };
        let saved = sm.do_suspend(self.pool);
        unpend_radio_irq();
        self.suspend_requested.store(false, Ordering::Release);
        saved
    }

    /// Restore ESB from saved state. Full RADIO re-init + PID/CRC restore.
    ///
    /// Must be called after `suspend()` or `try_suspend()`. Re-enables RADIO IRQ.
    /// Does NOT automatically start listening — call `start_listening()` after.
    pub fn restore(&self, state: &EsbSavedState, addresses: &EsbAddresses) {
        let sm = unsafe { &mut *self.sm.get() };
        sm.do_restore(state, addresses);
        self.suspend_requested.store(false, Ordering::Release);
        unpend_radio_irq();
        enable_radio_irq();
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
        let header = unsafe { self.pool.header(self.idx) };
        header.pipe
    }

    /// Get the RSSI value.
    pub fn rssi(&self) -> u8 {
        let header = unsafe { self.pool.header(self.idx) };
        header.rssi
    }

    /// Get the payload length.
    pub fn len(&self) -> usize {
        let header = unsafe { self.pool.header(self.idx) };
        header.length as usize
    }

    /// Check if the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the payload as a byte slice.
    pub fn payload(&self) -> &[u8] {
        let buf = unsafe { self.pool.buf(self.idx) };
        let header = unsafe { self.pool.header(self.idx) };
        let len = header.length as usize;
        let payload_offset = EsbHeader::PAYLOAD_OFFSET;
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
