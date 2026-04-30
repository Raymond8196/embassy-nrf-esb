//! ESB PTX and PRX state machines.
//!
//! Implements the ESB protocol state machines for Primary Transmitter (PTX)
//! and Primary Receiver (PRX) roles. All state machine logic runs in a single
//! ISR context (RADIO ISR) — the TIMER ISR only sets a flag and pends the
//! RADIO ISR (R9).
//!
//! The state machines are pure event processors: they receive event flags
//! rather than owning shared state. This avoids self-referential struct
//! issues and makes the code testable.
//!
//! Ref: esb-ng `src/irq.rs` lines 155–425.

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
/// Passed to the state machine from the ISR wrapper.
/// Ref: esb-ng `src/irq.rs` lines 105–108.
#[derive(Debug, Clone, Copy)]
pub struct IsrEvents {
    /// RADIO EVENTS_DISABLED fired.
    pub disabled: bool,
    /// TIMER ISR set the flag and pended RADIO ISR.
    pub timer: bool,
}

impl IsrEvents {
    /// Neither disabled nor timer — user-triggered event.
    pub fn user_event(self) -> bool {
        !self.disabled && !self.timer
    }
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
/// ISR context only (R9).
///
/// Ref: esb-ng `src/irq.rs` lines 191–310.
#[allow(dead_code)]
pub struct PtxStateMachine<T: TimerInstance> {
    pub(crate) radio: EsbRadio,
    pub(crate) timer: EsbTimer<T>,
    state: StatePtx,
    /// Retransmit attempt counter for the current packet.
    attempts: u8,
    /// TX pipe (typically 0 for PTX).
    tx_pipe: u8,
    /// Retransmit delay in µs (from config, minus ramp-up).
    retransmit_delay_us: u16,
    /// ACK timeout in µs (from config, plus ramp-up).
    ack_timeout_us: u16,
    /// Maximum retransmit attempts.
    max_attempts: u8,
    /// 2-bit packet identifier for duplicate detection (incremented per new packet).
    pid: u8,
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
        tx_pipe: u8,
    ) -> Self {
        Self {
            radio,
            timer,
            state: StatePtx::Idle,
            attempts: 0,
            tx_pipe,
            // Timer calculations (R8):
            // Retransmit: subtract ramp-up (radio re-enables from DISABLED)
            retransmit_delay_us: config.retransmit.delay_us.saturating_sub(RAMP_UP_US),
            // ACK timeout: add ramp-up (radio ramps to RX)
            ack_timeout_us: config.ack_timeout_us.saturating_add(RAMP_UP_US),
            max_attempts: config.retransmit.count,
            pid: 0,
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
    /// Returns events with disabled flag from radio, timer flag from caller.
    /// Ref: esb-ng `src/irq.rs` lines 135–152.
    pub fn check_events(&mut self, timer_flag: bool) -> IsrEvents {
        let disabled = self.radio.check_disabled_event();
        if disabled {
            self.radio.clear_disabled_event();
        }
        IsrEvents { disabled, timer: timer_flag }
    }

    /// Handle a RADIO ISR event.
    ///
    /// Must be called from the RADIO ISR only. Reads events, transitions
    /// state, and operates radio/timer registers inline (A3).
    ///
    /// `timer_flag` is the value of the shared timer AtomicBool, already
    /// loaded and cleared by the ISR wrapper.
    ///
    /// Ref: esb-ng `src/irq.rs` lines 196–294.
    pub fn handle_radio_event<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
        timer_flag: bool,
    ) -> PtxEvent {
        let evts = self.check_events(timer_flag);

        // If neither disabled nor timer, it's a user-triggered event
        // (e.g., new packet enqueued). Only valid in Idle state.
        if evts.user_event() && self.state != StatePtx::Idle {
            return PtxEvent::None;
        }

        match self.state {
            StatePtx::Idle => {
                self.send_next(pool);
                PtxEvent::None
            }

            StatePtx::TxNoAck => {
                // TX END for NoAck packet — release and send next
                // (esb-ng lines 215–219).
                self.radio.finish_tx_no_ack();
                self.release_tx(pool);
                self.advance_pid();
                self.send_next(pool);
                PtxEvent::None
            }

            StatePtx::Tx => {
                // TX END — prepare for ACK reception (esb-ng lines 220–244).
                debug_assert!(evts.disabled, "Tx: expected disabled event");

                let ack_idx = alloc_dma_buffer(pool);
                if let Some(idx) = ack_idx {
                    let dma_ptr = unsafe { pool.dma_ptr(idx) };
                    self.radio.prepare_for_ack(dma_ptr);
                    self.ack_rx_idx = idx;
                    self.state = StatePtx::WaitAck;
                } else {
                    self.radio.stop();
                    // Keep tx_idx set — send_next() will resend it
                    self.state = StatePtx::Idle;
                    return PtxEvent::None;
                }

                // Arm both timers (R8).
                self.timer.arm_retransmit(self.retransmit_delay_us);
                self.timer.arm_ack_timeout(self.ack_timeout_us);

                PtxEvent::None
            }

            StatePtx::WaitAck => {
                if evts.disabled {
                    // Got something — check CRC (esb-ng lines 249–259).
                    self.timer.disarm_ack_timeout();

                    if self.radio.check_ack() {
                        self.timer.disarm_retransmit();
                        self.radio.stop();
                        self.release_ack_rx(pool);
                        self.release_tx(pool);
                        self.attempts = 0;
                        self.advance_pid();
                        self.send_next(pool);
                        return PtxEvent::None;
                    }
                    // CRC mismatch — stop radio, release ACK buffer, fall through
                    self.radio.stop();
                    self.release_ack_rx(pool);
                } else if evts.timer {
                    // ACK timeout (esb-ng line 263).
                    // Don't disarm retransmit timer — it's still counting down
                    // and will trigger the WaitRetransmit state.
                    self.timer.disarm_ack_timeout();
                    self.radio.stop();
                    self.release_ack_rx(pool);
                } else {
                    return PtxEvent::None;
                }

                // Retransmit needed — check max attempts FIRST (esb-ng line 270).
                // R6: use >= not > — esb-ng uses > giving one extra attempt.
                self.attempts += 1;
                if self.attempts >= self.max_attempts {
                    // Max reached — drop packet, try next
                    self.release_tx(pool);
                    self.attempts = 0;
                    self.advance_pid();
                    self.send_next(pool);
                    return PtxEvent::MaxAttempts;
                }

                // Retransmit — wait for timer (radio already stopped above)
                self.state = StatePtx::WaitRetransmit;

                PtxEvent::None
            }

            StatePtx::WaitRetransmit => {
                debug_assert!(evts.timer, "WaitRetransmit: expected timer event");
                self.retransmit(pool);
                PtxEvent::None
            }
        }
    }

    /// Send the next queued packet, or go idle if queue is empty.
    /// If tx_idx is already set (from a previous failed ACK buffer alloc),
    /// resend that packet instead of dequeuing a new one.
    /// Ref: esb-ng `src/irq.rs` lines 296–309.
    fn send_next<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        // If a TX packet is pending from a failed ACK buffer allocation,
        // resend it (tx_idx is still in IN_DMA state).
        if self.tx_idx != NO_IDX {
            let dma_ptr = unsafe { pool.dma_ptr(self.tx_idx) };
            self.radio.transmit(self.tx_pipe, dma_ptr, true);
            self.state = StatePtx::Tx;
            return;
        }

        if let Some(idx) = pool.try_dequeue_tx() {
            let header = unsafe { pool.header_mut(idx) };
            let no_ack = header.no_ack();
            // Set PID in header before TX (S1 field bits 2:1).
            header.set_pid(self.pid);
            let dma_ptr = unsafe { pool.dma_ptr(idx) };
            pool.tx_to_dma(idx);
            self.tx_idx = idx;
            self.radio.transmit(self.tx_pipe, dma_ptr, !no_ack);
            self.state = if no_ack { StatePtx::TxNoAck } else { StatePtx::Tx };
        } else {
            self.radio.disable_disabled_interrupt();
            self.state = StatePtx::Idle;
        }
    }

    /// Advance PID after a packet is fully processed (ACK received,
    /// max retransmit reached, or NoAck TX completed).
    fn advance_pid(&mut self) {
        self.pid = (self.pid + 1) & 0x03;
    }

    /// Retransmit the current TX packet.
    fn retransmit<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        if self.tx_idx != NO_IDX {
            // SAFETY: tx_idx is in IN_DMA state; buffer data is still valid.
            let dma_ptr = unsafe { pool.dma_ptr(self.tx_idx) };
            self.radio.transmit(self.tx_pipe, dma_ptr, true);
            self.state = StatePtx::Tx;
        } else {
            self.radio.disable_disabled_interrupt();
            self.state = StatePtx::Idle;
        }
    }

    /// Release the current TX buffer back to the pool.
    fn release_tx<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        if self.tx_idx != NO_IDX {
            pool.release_tx(self.tx_idx);
            self.tx_idx = NO_IDX;
        }
    }

    /// Release the current ACK RX buffer back to the pool.
    fn release_ack_rx<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        if self.ack_rx_idx != NO_IDX {
            pool.release_rx(self.ack_rx_idx);
            self.ack_rx_idx = NO_IDX;
        }
    }

    /// Handle TIMER ISR — clear events, pend RADIO ISR.
    /// Ref: esb-ng `src/irq.rs` lines 49–66.
    pub fn handle_timer_event(&self) {
        if self.timer.is_retransmit_fired() {
            self.timer.disarm_retransmit();
        }
        if self.timer.is_ack_timeout_fired() {
            self.timer.disarm_ack_timeout();
        }
        pend_radio_isr();
    }

    /// Trigger a send from application context (pend RADIO ISR).
    pub fn trigger_send(&self) {
        pend_radio_isr();
    }
}

// ---- PRX State Machine ----

/// PRX (Primary Receiver) state machine.
/// Ref: esb-ng `src/irq.rs` lines 312–425.
#[allow(dead_code)]
pub struct PrxStateMachine<T: TimerInstance> {
    pub(crate) radio: EsbRadio,
    pub(crate) timer: EsbTimer<T>,
    state: StatePrx,
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
        enabled_pipes: u8,
    ) -> Self {
        let _ = config;
        Self {
            radio,
            timer,
            state: StatePrx::Idle,
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
    pub fn check_events(&mut self, timer_flag: bool) -> IsrEvents {
        let disabled = self.radio.check_disabled_event();
        if disabled {
            self.radio.clear_disabled_event();
        }
        IsrEvents { disabled, timer: timer_flag }
    }

    /// Start receiving on enabled pipes.
    /// Ref: esb-ng `src/irq.rs` lines 386–395.
    pub fn start_receiving<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) -> Result<(), Error> {
        if self.state != StatePrx::Idle {
            return Ok(());
        }

        let idx = alloc_dma_buffer(pool).ok_or(Error::OutOfMemory)?;
        self.rx_idx = idx;
        let dma_ptr = unsafe { pool.dma_ptr(idx) };
        self.radio.start_receiving(self.enabled_pipes, dma_ptr);
        self.state = StatePrx::Receiver;
        Ok(())
    }

    /// Handle a RADIO ISR event.
    /// Ref: esb-ng `src/irq.rs` lines 316–383.
    pub fn handle_radio_event<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
        timer_flag: bool,
    ) -> (PrxEvent, Option<usize>) {
        let evts = self.check_events(timer_flag);

        if evts.user_event() && self.state != StatePrx::Idle {
            return (PrxEvent::None, None);
        }

        match self.state {
            StatePrx::Receiver => {
                debug_assert!(evts.disabled, "Receiver: expected disabled event");

                match self.radio.check_packet() {
                    RxResult::BadCrc => {
                        // Bad CRC — radio already restarted with same PACKETPTR.
                        // Keep using the same DMA buffer.
                        (PrxEvent::BadCrc, None)
                    }
                    RxResult::NewPacket => {
                        let pipe = self.radio.rx_match() as usize;
                        let crc = self.radio.rx_crc();
                        let rssi = self.radio.rssi_sample();

                        let rx_idx = self.rx_idx;
                        let header = unsafe { pool.header_mut(rx_idx) };
                        let pid = header.pid();
                        let no_ack = header.no_ack();

                        let is_dup = self.radio.check_duplicate(pipe, pid, crc);

                        if is_dup {
                            if no_ack {
                                // Duplicate NoAck — restart RX with new buffer
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
                                // Duplicate with ACK — send repeated fallback ACK.
                                // The original rx_idx buffer is no longer used by DMA
                                // (PACKETPTR was changed by setup_ack_tx_fallback).
                                // Release it before entering TxRepeatedAck.
                                pool.release_rx(rx_idx);
                                self.rx_idx = NO_IDX;
                                self.radio.setup_ack_tx_fallback(pipe as u8);
                                self.state = StatePrx::TxRepeatedAck;
                                return (PrxEvent::Duplicate, None);
                            }
                        }

                        // New packet — update detection, write metadata
                        self.radio.update_detection(pipe, pid, crc);
                        let header = unsafe { pool.header_mut(rx_idx) };
                        header.rssi = rssi;
                        header.pipe = pipe as u8;

                        if no_ack {
                            // NoAck: deliver to app, no ACK
                            pool.rx_complete(rx_idx);
                            self.rx_idx = NO_IDX;

                            match alloc_dma_buffer(pool) {
                                Some(new_idx) => {
                                    self.rx_idx = new_idx;
                                    let dma_ptr = unsafe { pool.dma_ptr(new_idx) };
                                    self.radio.complete_rx_no_ack(dma_ptr);
                                }
                                None => {
                                    // No buffer available — stop radio and go idle
                                    self.radio.stop();
                                    self.state = StatePrx::Idle;
                                }
                            }
                            return (PrxEvent::ReceivedNoAck, Some(rx_idx));
                        }

                        // Need ACK
                        self.setup_ack_tx(pool, pipe as u8);
                        self.pending_rx_idx = rx_idx;
                        self.rx_idx = NO_IDX;
                        self.state = StatePrx::TxAck;
                        (PrxEvent::Received, Some(rx_idx))
                    }
                }
            }

            StatePrx::TxAck => {
                debug_assert!(evts.disabled, "TxAck: expected disabled event");

                if self.pending_rx_idx != NO_IDX {
                    pool.rx_complete(self.pending_rx_idx);
                    self.pending_rx_idx = NO_IDX;
                }

                match alloc_dma_buffer(pool) {
                    Some(idx) => {
                        self.rx_idx = idx;
                        let dma_ptr = unsafe { pool.dma_ptr(idx) };
                        self.radio.complete_rx_ack(dma_ptr);
                        self.state = StatePrx::Receiver;
                    }
                    None => {
                        // No buffer — stop radio and go idle
                        self.radio.stop();
                        self.state = StatePrx::Idle;
                        return (PrxEvent::None, None);
                    }
                }
                (PrxEvent::None, None)
            }

            StatePrx::TxRepeatedAck => {
                debug_assert!(evts.disabled, "TxRepeatedAck: expected disabled event");

                match alloc_dma_buffer(pool) {
                    Some(idx) => {
                        self.rx_idx = idx;
                        let dma_ptr = unsafe { pool.dma_ptr(idx) };
                        self.radio.complete_rx_ack(dma_ptr);
                        self.state = StatePrx::Receiver;
                    }
                    None => {
                        // No buffer — stop radio and go idle
                        self.radio.stop();
                        self.state = StatePrx::Idle;
                        return (PrxEvent::None, None);
                    }
                }
                (PrxEvent::None, None)
            }

            StatePrx::Idle => {
                debug_assert!(evts.user_event(), "Idle: expected user event");
                let _ = self.start_receiving(pool);
                (PrxEvent::None, None)
            }
        }
    }

    /// Set up ACK TX for a new (non-duplicate) packet.
    fn setup_ack_tx<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
        pipe: u8,
    ) {
        if let Some(idx) = pool.try_dequeue_tx() {
            let dma_ptr = unsafe { pool.dma_ptr(idx) };
            pool.tx_to_dma(idx);
            self.radio.setup_ack_tx(pipe, dma_ptr);
        } else {
            self.radio.setup_ack_tx_fallback(pipe);
        }
    }

    /// Handle TIMER ISR — defensively clear any stale timer events.
    /// PRX doesn't use timers in normal operation, so we don't pend
    /// the RADIO ISR (unlike PTX). A stale timer event should not
    /// trigger state machine processing.
    pub fn handle_timer_event(&self) {
        if self.timer.is_retransmit_fired() {
            self.timer.disarm_retransmit();
        }
        if self.timer.is_ack_timeout_fired() {
            self.timer.disarm_ack_timeout();
        }
    }

    /// Stop receiving and go idle.
    /// Ref: esb-ng `src/irq.rs` lines 398–406.
    pub fn stop_receiving<const N: usize, const SIZE: usize>(
        &mut self,
        pool: &PacketPool<N, SIZE>,
    ) {
        self.radio.stop();
        self.timer.disarm_retransmit();
        self.timer.disarm_ack_timeout();
        let _ = self.check_events(false);

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
fn alloc_dma_buffer<const N: usize, const SIZE: usize>(
    pool: &PacketPool<N, SIZE>,
) -> Option<usize> {
    (0..N).find(|&i| pool.rx_to_dma(i))
}

/// Pend the RADIO ISR.
#[cfg(any(feature = "nrf52840", feature = "nrf52833", feature = "nrf52832"))]
fn pend_radio_isr() {
    cortex_m::peripheral::NVIC::pend(crate::pac::Interrupt::RADIO);
}
