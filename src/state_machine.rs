//! ESB PTX and PRX state machines.
//!
//! Implements the ESB protocol state machines for Primary Transmitter (PTX)
//! and Primary Receiver (PRX) roles. All state machine logic runs in a single
//! ISR context (RADIO ISR) — the TIMER ISR only sets a flag and pends the
//! RADIO ISR (R9).
//!
//! Ref: esb-ng `src/irq.rs` lines 155–425.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::config::{EsbConfig, RAMP_UP_US};
use crate::error::Error;
use crate::payload::PacketPool;
use crate::radio::{EsbRadio, RxResult};
use crate::timer::EsbTimer;

use crate::timer::TimerInstance;

// ---- PTX States ----

/// PTX state machine states.
///
/// Ref: esb-ng `src/irq.rs` lines 17–28.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum StatePtx {
    /// Radio idle — waiting for TX queue entry.
    #[default]
    Idle,
    /// Transmitting a packet (ACK requested).
    Tx,
    /// Transmitting a NoAck packet (no ACK expected).
    TxNoAck,
    /// Waiting for ACK response after TX.
    WaitAck,
    /// Waiting for retransmit timeout.
    WaitRetransmit,
}

// ---- PRX States ----

/// PRX state machine states.
///
/// Ref: esb-ng `src/irq.rs` lines 31–41.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum StatePrx {
    /// Radio idle — not listening.
    #[default]
    Idle,
    /// Listening for incoming packets.
    Receiver,
    /// Transmitting ACK for a new packet.
    TxAck,
    /// Transmitting ACK for a duplicate (repeated) packet.
    TxRepeatedAck,
}

// ---- ISR Event Flags ----

/// Events checked at the start of each ISR invocation.
///
/// Ref: esb-ng `src/irq.rs` lines 105–108.
#[derive(Debug, Clone, Copy)]
struct IsrEvents {
    /// RADIO EVENTS_DISABLED fired.
    disabled: bool,
    /// TIMER ISR set the flag and pended RADIO ISR.
    timer: bool,
}

/// Result of PTX event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PtxEvent {
    /// No special event — state machine progressed normally.
    None,
    /// Max retransmit attempts reached — packet dropped.
    MaxAttempts,
}

/// Result of PRX event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PrxEvent {
    /// No special event.
    None,
    /// New valid packet received and queued.
    Received,
    /// Duplicate packet detected — ACK sent but no new data.
    Duplicate,
    /// NoAck packet received — queued but no ACK sent.
    ReceivedNoAck,
    /// CRC check failed — radio auto-restarted.
    BadCrc,
}

/// Sentinel value meaning "no active DMA buffer".
const NO_IDX: usize = usize::MAX;

// ---- PTX State Machine ----

/// PTX (Primary Transmitter) state machine.
///
/// Holds radio, timer, and protocol state. All methods are called from
/// ISR context only (R9). The `timer_flag` is set by the TIMER ISR and
/// read/cleared in the RADIO ISR.
///
/// Ref: esb-ng `src/irq.rs` lines 191–310.
#[allow(dead_code)]
pub struct PtxStateMachine<T: TimerInstance> {
    radio: EsbRadio,
    timer: EsbTimer<T>,
    state: StatePtx,
    /// Retransmit attempt counter for the current packet.
    attempts: u8,
    /// Shared flag set by TIMER ISR, cleared by RADIO ISR.
    timer_flag: &'static AtomicBool,
    /// TX pipe (typically 0 for PTX).
    tx_pipe: u8,
    /// Retransmit delay in µs (from config, minus ramp-up).
    retransmit_delay_us: u16,
    /// ACK timeout in µs (from config, plus ramp-up).
    ack_timeout_us: u16,
    /// Maximum retransmit attempts.
    max_attempts: u8,
    /// Current TX packet pool index (IN_DMA while transmitting).
    tx_idx: usize,
    /// Current ACK RX buffer pool index (IN_DMA while waiting for ACK).
    ack_rx_idx: usize,
}

#[allow(dead_code)]
impl<T: TimerInstance> PtxStateMachine<T> {
    /// Create a new PTX state machine.
    pub(crate) fn new(
        radio: EsbRadio,
        timer: EsbTimer<T>,
        config: &EsbConfig,
        timer_flag: &'static AtomicBool,
        tx_pipe: u8,
    ) -> Self {
        Self {
            radio,
            timer,
            state: StatePtx::Idle,
            attempts: 0,
            timer_flag,
            tx_pipe,
            // Timer calculations (R8):
            // Retransmit: subtract ramp-up (radio re-enables from DISABLED)
            retransmit_delay_us: config.retransmit.delay_us.saturating_sub(RAMP_UP_US),
            // ACK timeout: add ramp-up (radio ramps to RX)
            ack_timeout_us: config.ack_timeout_us.saturating_add(RAMP_UP_US),
            max_attempts: config.retransmit.count,
            tx_idx: NO_IDX,
            ack_rx_idx: NO_IDX,
        }
    }

    /// Get current PTX state.
    pub fn state(&self) -> StatePtx {
        self.state
    }

    /// Check and clear ISR event flags.
    ///
    /// Ref: esb-ng `src/irq.rs` lines 135–152.
    fn check_events(&mut self) -> IsrEvents {
        let evts = IsrEvents {
            disabled: self.radio.check_disabled_event(),
            timer: self.timer_flag.load(Ordering::Acquire),
        };

        if evts.disabled {
            self.radio.clear_disabled_event();
        }
        if evts.timer {
            self.timer_flag.store(false, Ordering::Release);
        }

        evts
    }

    /// Handle a RADIO ISR event.
    ///
    /// Must be called from the RADIO ISR only. Reads events, transitions
    /// state, and operates radio/timer registers inline (A3).
    ///
    /// Ref: esb-ng `src/irq.rs` lines 196–294.
    pub fn handle_radio_event<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) -> PtxEvent {
        let evts = self.check_events();

        // If neither disabled nor timer, it's a user-triggered event
        // (e.g., new packet enqueued). Only valid in Idle state.
        let user_event = !evts.disabled && !evts.timer;

        if user_event && self.state != StatePtx::Idle {
            return PtxEvent::None;
        }

        match self.state {
            StatePtx::Idle => {
                // User pushed a packet — start transmitting (esb-ng line 212).
                self.send_next(pool);
                PtxEvent::None
            }

            StatePtx::TxNoAck => {
                // TX END for NoAck packet — release and send next
                // (esb-ng lines 215–219).
                self.radio.finish_tx_no_ack();

                // Release the TX buffer back to pool
                if self.tx_idx != NO_IDX {
                    pool.release_tx(self.tx_idx);
                    self.tx_idx = NO_IDX;
                }

                self.send_next(pool);
                PtxEvent::None
            }

            StatePtx::Tx => {
                // TX END — prepare for ACK reception (esb-ng lines 220–244).
                debug_assert!(evts.disabled, "Tx: expected disabled event");

                // Allocate a free buffer for ACK DMA
                let ack_idx = alloc_dma_buffer(pool);
                if let Some(idx) = ack_idx {
                    // SAFETY: We just allocated this; it's in IN_DMA state.
                    let dma_ptr = unsafe { pool.dma_ptr(idx) };
                    self.radio.prepare_for_ack(dma_ptr);
                    self.ack_rx_idx = idx;
                    self.state = StatePtx::WaitAck;
                } else {
                    // No buffer available — stop and go idle (esb-ng line 231).
                    self.radio.stop();
                    if self.tx_idx != NO_IDX {
                        pool.release_tx(self.tx_idx);
                        self.tx_idx = NO_IDX;
                    }
                    self.state = StatePtx::Idle;
                    return PtxEvent::None;
                }

                // Arm both timers (esb-ng lines 238–243).
                // Retransmit: absolute (clear+start), value = delay - RAMP_UP.
                self.timer.arm_retransmit(self.retransmit_delay_us);
                // ACK timeout: relative (capture+add), value = timeout + RAMP_UP.
                self.timer.arm_ack_timeout(self.ack_timeout_us);

                PtxEvent::None
            }

            StatePtx::WaitAck => {
                let mut retransmit = false;

                if evts.disabled {
                    // Got something — check CRC (esb-ng lines 249–259).
                    self.timer.disarm_ack_timeout();

                    // Release the ACK RX buffer
                    if self.ack_rx_idx != NO_IDX {
                        pool.release_rx(self.ack_rx_idx);
                        self.ack_rx_idx = NO_IDX;
                    }

                    if self.radio.check_ack() {
                        // ACK received successfully
                        self.timer.disarm_retransmit();

                        // Release the TX buffer
                        if self.tx_idx != NO_IDX {
                            pool.release_tx(self.tx_idx);
                            self.tx_idx = NO_IDX;
                        }

                        self.attempts = 0;
                        self.send_next(pool);
                        return PtxEvent::None;
                    } else {
                        // CRC mismatch — retransmit
                        retransmit = true;
                    }
                } else if evts.timer {
                    // ACK timeout (esb-ng line 263).
                    // Release the ACK RX buffer
                    if self.ack_rx_idx != NO_IDX {
                        pool.release_rx(self.ack_rx_idx);
                        self.ack_rx_idx = NO_IDX;
                    }
                    retransmit = true;
                }

                if retransmit {
                    self.radio.stop();
                    self.attempts += 1;
                    self.state = StatePtx::WaitRetransmit;
                }

                // Check max attempts (R6: use >= not >).
                // esb-ng uses `>` which gives one extra attempt.
                if self.attempts >= self.max_attempts {
                    self.timer.disarm_retransmit();

                    // Drop current TX packet
                    if self.tx_idx != NO_IDX {
                        pool.release_tx(self.tx_idx);
                        self.tx_idx = NO_IDX;
                    }
                    self.attempts = 0;
                    self.send_next(pool);
                    return PtxEvent::MaxAttempts;
                }

                PtxEvent::None
            }

            StatePtx::WaitRetransmit => {
                // Retransmit timer fired — send again (esb-ng lines 283–291).
                debug_assert!(evts.timer, "WaitRetransmit: expected timer event");
                self.retransmit(pool);
                PtxEvent::None
            }
        }
    }

    /// Send the next queued packet, or go idle if queue is empty.
    ///
    /// Ref: esb-ng `src/irq.rs` lines 296–309.
    fn send_next<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        if let Some(idx) = pool.try_dequeue_tx() {
            // SAFETY: We just dequeued this; it's in TX_QUEUED state.
            let header = unsafe { pool.header_mut(idx) };
            let no_ack = header.no_ack();
            let dma_ptr = unsafe { pool.dma_ptr(idx) };

            pool.tx_to_dma(idx);
            self.tx_idx = idx;

            self.radio.transmit(self.tx_pipe, dma_ptr, !no_ack);

            if no_ack {
                self.state = StatePtx::TxNoAck;
            } else {
                self.state = StatePtx::Tx;
            }
        } else {
            // No packet to send — disable interrupt, go idle
            // (esb-ng lines 306–308).
            self.radio.disable_disabled_interrupt();
            self.state = StatePtx::Idle;
        }
    }

    /// Retransmit the current TX packet.
    ///
    /// Unlike send_next, this re-sends the same packet that's still
    /// held in `tx_idx`.
    fn retransmit<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        if self.tx_idx != NO_IDX {
            // SAFETY: tx_idx is in IN_DMA state, we're re-transmitting.
            let dma_ptr = unsafe { pool.dma_ptr(self.tx_idx) };

            // Re-transmit on same pipe
            self.radio.transmit(self.tx_pipe, dma_ptr, true);
            self.state = StatePtx::Tx;
        } else {
            // Shouldn't happen, but recover gracefully
            self.radio.disable_disabled_interrupt();
            self.state = StatePtx::Idle;
        }
    }

    /// Trigger a send from application context (pend RADIO ISR).
    pub fn trigger_send(&self) {
        pend_radio_isr();
    }

    /// Handle TIMER ISR — minimal: clear events, set flag, pend RADIO ISR.
    ///
    /// Ref: esb-ng `src/irq.rs` lines 49–66.
    pub fn handle_timer_event(&self) {
        // Clear timer interrupt events
        if self.timer.is_retransmit_fired() {
            self.timer.disarm_retransmit();
        }
        // Retransmit might have fired just after ack timeout — clear both.
        if self.timer.is_ack_timeout_fired() {
            self.timer.disarm_ack_timeout();
        }

        // Set flag for RADIO ISR to read (esb-ng line 63).
        self.timer_flag.store(true, Ordering::Release);

        // Pend RADIO ISR — all state machine logic runs there (R9).
        pend_radio_isr();
    }
}

// ---- PRX State Machine ----

/// PRX (Primary Receiver) state machine.
///
/// Ref: esb-ng `src/irq.rs` lines 312–425.
#[allow(dead_code)]
pub struct PrxStateMachine<T: TimerInstance> {
    radio: EsbRadio,
    timer: EsbTimer<T>,
    state: StatePrx,
    /// Shared flag set by TIMER ISR (unused in PRX, kept for API symmetry).
    timer_flag: &'static AtomicBool,
    /// Enabled pipe bitmask.
    enabled_pipes: u8,
    /// Current RX DMA buffer pool index.
    rx_idx: usize,
    /// Current received packet pool index (queued for app after RX).
    pending_rx_idx: usize,
}

#[allow(dead_code)]
impl<T: TimerInstance> PrxStateMachine<T> {
    /// Create a new PRX state machine.
    pub(crate) fn new(
        radio: EsbRadio,
        timer: EsbTimer<T>,
        config: &EsbConfig,
        timer_flag: &'static AtomicBool,
        enabled_pipes: u8,
    ) -> Self {
        let _ = config;
        Self {
            radio,
            timer,
            state: StatePrx::Idle,
            timer_flag,
            enabled_pipes,
            rx_idx: NO_IDX,
            pending_rx_idx: NO_IDX,
        }
    }

    /// Get current PRX state.
    pub fn state(&self) -> StatePrx {
        self.state
    }

    /// Check and clear ISR event flags.
    fn check_events(&mut self) -> IsrEvents {
        let evts = IsrEvents {
            disabled: self.radio.check_disabled_event(),
            timer: self.timer_flag.load(Ordering::Acquire),
        };

        if evts.disabled {
            self.radio.clear_disabled_event();
        }
        if evts.timer {
            self.timer_flag.store(false, Ordering::Release);
        }

        evts
    }

    /// Start receiving on enabled pipes.
    ///
    /// Allocates an RX buffer from the pool and starts the RADIO.
    /// Ref: esb-ng `src/irq.rs` lines 386–395.
    pub fn start_receiving<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) -> Result<(), Error> {
        if self.state != StatePrx::Idle {
            return Ok(());
        }

        // Allocate an RX buffer
        let idx = alloc_dma_buffer(pool).ok_or(Error::OutOfMemory)?;
        self.rx_idx = idx;
        // SAFETY: We just allocated this; it's in IN_DMA state.
        let dma_ptr = unsafe { pool.dma_ptr(idx) };

        self.radio.start_receiving(self.enabled_pipes, dma_ptr);
        self.state = StatePrx::Receiver;
        Ok(())
    }

    /// Handle a RADIO ISR event.
    ///
    /// Ref: esb-ng `src/irq.rs` lines 316–383.
    pub fn handle_radio_event<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) -> (PrxEvent, Option<usize>) {
        let evts = self.check_events();

        let user_event = !evts.disabled && !evts.timer;
        if user_event && self.state != StatePrx::Idle {
            return (PrxEvent::None, None);
        }

        match self.state {
            StatePrx::Receiver => {
                debug_assert!(evts.disabled, "Receiver: expected disabled event");

                // Check received packet (esb-ng lines 330–351).
                match self.radio.check_packet() {
                    RxResult::BadCrc => {
                        // Bad CRC — radio already restarted RX with the same
                        // PACKETPTR (esb-ng lines 345–354). Keep using the same
                        // DMA buffer — RADIO will overwrite it on next RX.
                        (PrxEvent::BadCrc, None)
                    }
                    RxResult::NewPacket => {
                        let pipe = self.radio.rx_match() as usize;
                        let crc = self.radio.rx_crc();
                        let rssi = self.radio.rssi_sample();

                        // SAFETY: rx_idx is in IN_DMA state, we read header fields.
                        let rx_idx = self.rx_idx;
                        let header = unsafe { pool.header_mut(rx_idx) };
                        let pid = header.pid();
                        let no_ack = header.no_ack();

                        // Duplicate detection
                        let is_dup = self.radio.check_duplicate(pipe, pid, crc);

                        if is_dup {
                            if no_ack {
                                // Duplicate NoAck — just restart RX
                                // (esb-ng lines 341–344).
                                match alloc_dma_buffer(pool) {
                                    Some(new_idx) => {
                                        pool.release_rx(rx_idx);
                                        self.rx_idx = new_idx;
                                        let dma_ptr = unsafe { pool.dma_ptr(new_idx) };
                                        self.radio.complete_rx_no_ack(dma_ptr);
                                    }
                                    None => {
                                        // Re-use current buffer
                                        let dma_ptr = unsafe { pool.dma_ptr(rx_idx) };
                                        self.radio.complete_rx_no_ack(dma_ptr);
                                    }
                                }
                                return (PrxEvent::Duplicate, None);
                            } else {
                                // Duplicate with ACK — send repeated ACK
                                // (esb-ng line 349).
                                self.radio.setup_ack_tx_fallback(pipe as u8);
                                self.state = StatePrx::TxRepeatedAck;
                                return (PrxEvent::Duplicate, None);
                            }
                        }

                        // New packet — update detection state
                        self.radio.update_detection(pipe, pid, crc);

                        // Write metadata to header
                        let header = unsafe { pool.header_mut(rx_idx) };
                        header.rssi = rssi;
                        header.pipe = pipe as u8;

                        if no_ack {
                            // NoAck: deliver to app but don't send ACK
                            // (esb-ng lines 335–339).
                            pool.rx_complete(rx_idx);
                            self.rx_idx = NO_IDX;

                            // Allocate new RX buffer and restart
                            match alloc_dma_buffer(pool) {
                                Some(new_idx) => {
                                    self.rx_idx = new_idx;
                                    let dma_ptr = unsafe { pool.dma_ptr(new_idx) };
                                    self.radio.complete_rx_no_ack(dma_ptr);
                                }
                                None => {
                                    self.state = StatePrx::Idle;
                                }
                            }
                            return (PrxEvent::ReceivedNoAck, Some(rx_idx));
                        }

                        // Need ACK — set up ACK TX
                        self.setup_ack_tx(pool, pipe as u8);
                        self.pending_rx_idx = rx_idx;
                        self.rx_idx = NO_IDX;
                        self.state = StatePrx::TxAck;
                        (PrxEvent::Received, Some(rx_idx))
                    }
                    // RxResult::Duplicate is never returned by check_packet();
                    // duplicate detection is done via check_duplicate() above.
                    RxResult::Duplicate => (PrxEvent::None, None),
                }
            }

            StatePrx::TxAck => {
                // ACK TX completed — set up next RX (esb-ng lines 353–362).
                debug_assert!(evts.disabled, "TxAck: expected disabled event");

                // Deliver received packet to app
                if self.pending_rx_idx != NO_IDX {
                    pool.rx_complete(self.pending_rx_idx);
                    self.pending_rx_idx = NO_IDX;
                }

                // Allocate new RX buffer and restart RX
                match alloc_dma_buffer(pool) {
                    Some(idx) => {
                        self.rx_idx = idx;
                        let dma_ptr = unsafe { pool.dma_ptr(idx) };
                        self.radio.complete_rx_ack(dma_ptr);
                    }
                    None => {
                        self.state = StatePrx::Idle;
                        return (PrxEvent::None, None);
                    }
                }
                self.state = StatePrx::Receiver;
                (PrxEvent::None, None)
            }

            StatePrx::TxRepeatedAck => {
                // Repeated ACK TX completed (esb-ng lines 363–372).
                debug_assert!(evts.disabled, "TxRepeatedAck: expected disabled event");

                // Allocate new RX buffer and restart RX
                match alloc_dma_buffer(pool) {
                    Some(idx) => {
                        self.rx_idx = idx;
                        let dma_ptr = unsafe { pool.dma_ptr(idx) };
                        self.radio.complete_rx_ack(dma_ptr);
                    }
                    None => {
                        self.state = StatePrx::Idle;
                        return (PrxEvent::None, None);
                    }
                }
                self.state = StatePrx::Receiver;
                (PrxEvent::None, None)
            }

            StatePrx::Idle => {
                debug_assert!(user_event, "Idle: expected user event");
                // User triggered — start receiving (esb-ng lines 373–380).
                let _ = self.start_receiving(pool);
                (PrxEvent::None, None)
            }
        }
    }

    /// Set up ACK TX for a new (non-duplicate) packet.
    ///
    /// Checks if there's a queued TX payload to send as ACK. If not,
    /// uses the fallback empty ACK `[0, 0]` (R11).
    fn setup_ack_tx<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
        pipe: u8,
    ) {
        if let Some(idx) = pool.try_dequeue_tx() {
            // SAFETY: We just dequeued this; it's ours.
            let dma_ptr = unsafe { pool.dma_ptr(idx) };
            pool.tx_to_dma(idx);
            self.radio.setup_ack_tx(pipe, dma_ptr);
        } else {
            // No ACK payload queued — use fallback (esb-ng line 377, R11).
            self.radio.setup_ack_tx_fallback(pipe);
        }
    }

    /// Handle TIMER ISR (minimal — PRX doesn't typically use timer).
    pub fn handle_timer_event(&self) {
        // PRX doesn't use the timer in normal operation.
        // Just clear any pending events.
        if self.timer.is_retransmit_fired() {
            self.timer.disarm_retransmit();
        }
        if self.timer.is_ack_timeout_fired() {
            self.timer.disarm_ack_timeout();
        }
    }

    /// Stop receiving and go idle.
    ///
    /// Ref: esb-ng `src/irq.rs` lines 398–406.
    pub fn stop_receiving<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        self.radio.stop();
        self.timer.disarm_retransmit();
        self.timer.disarm_ack_timeout();
        let _ = self.check_events();

        // Release any held buffers
        if self.rx_idx != NO_IDX {
            pool.release_rx(self.rx_idx);
            self.rx_idx = NO_IDX;
        }
        if self.pending_rx_idx != NO_IDX {
            pool.release_rx(self.pending_rx_idx);
            self.pending_rx_idx = NO_IDX;
        }

        self.state = StatePrx::Idle;
    }
}

// ---- Helpers ----

/// Allocate a free buffer and transition it to DMA state.
///
/// Searches for a FREE slot, transitions it to IN_DMA.
/// Returns the pool index, or None if pool is exhausted.
#[allow(clippy::manual_find)]
fn alloc_dma_buffer<const N: usize, const SIZE: usize>(
    pool: &PacketPool<N, SIZE>,
) -> Option<usize> {
    (0..N).find(|&i| pool.rx_to_dma(i))
}

/// Pend the RADIO ISR — used to trigger RADIO ISR from TIMER ISR or app.
#[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
fn pend_radio_isr() {
    cortex_m::peripheral::NVIC::pend(crate::pac::Interrupt::RADIO);
}
