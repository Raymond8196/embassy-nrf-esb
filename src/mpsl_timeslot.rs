//! MPSL timeslot adapter for ESB.
//!
//! Manages timeslot sessions via nrf-mpsl, providing async APIs for
//! requesting individual timeslots, chained timeslot sequences, and
//! PTX transmissions within timeslots.
//!
//! # Stability
//!
//! This module is experimental and diagnostic. The current public functions are
//! useful for bring-up and hardware regression runs, but they are not the
//! stable RMK split-transport API.
//!
//! Current limitations:
//!
//! - sessions are free functions backed by static global state;
//! - PTX/PRX buffers are fixed static buffers rather than caller-owned pools;
//! - the in-slot PTX/PRX protocol logic is duplicated from the exclusive ESB
//!   state machines;
//! - the diagnostic PTX path does not yet implement the full ACK timeout and
//!   retry behavior used by the exclusive ESB core;
//! - active BLE connection coexistence still needs scheduler tuning.
//!
//! Use the exclusive `EsbPtx`/`EsbPrx` path as the stable baseline for the
//! first RMK dongle prototype.

use core::cell::RefCell;
use core::future::poll_fn;
use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};
use core::task::Poll;

use cortex_m::peripheral::NVIC;
use embassy_nrf::interrupt::Interrupt;
use embassy_nrf::pac;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::waitqueue::WakerRegistration;

use nrf_mpsl::{MultiprotocolServiceLayer, RetVal, raw};

use crate::error::Error;
use crate::mpsl_common::{
    NUM_PIPES, advance_pid, delta_per_pipe, next_pipe_in_mask, read_counter_payload,
    write_counter_packet, write_counter_payload_packet, write_counter_schedule_packet,
};
pub use crate::mpsl_profile::{
    BleCoexistenceHint, CoexistenceProfile, CoexistenceProfileConfig, PrxScheduleConfig,
    PrxSlotConfig, PtxEventConfig, PtxPollConfig, PtxScheduleGateConfig, PtxScheduleMode,
    TimeslotRequestConfig,
};
pub use crate::mpsl_radio::{RadioDisableResult, RadioQuiesceResult, RadioRecoveryPolicy};
use crate::mpsl_radio::{disable_radio_bounded, quiesce_radio_before_timeslot_end};
use crate::mpsl_schedule::{
    ScheduleHint, ScheduleTracker, ScheduleTrackerSnapshot, decode_counter_payload_schedule_hint,
};

const TIMESLOT_TIMER_INTERRUPT: Interrupt = Interrupt::TIMER0;

const TIMESLOT_HFCLK: u8 = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
const TIMESLOT_PRIORITY_NORMAL: u8 = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
const TIMESLOT_PRIORITY_HIGH: u8 = raw::MPSL_TIMESLOT_PRIORITY_HIGH as u8;

fn mpsl_ok(ret: i32) -> Result<(), Error> {
    RetVal::from(ret)
        .to_result()
        .map(|_| ())
        .map_err(|_| Error::Mpsl)
}

fn configure_earliest_request(
    request: &mut raw::mpsl_timeslot_request_t,
    priority: u8,
    length_us: u32,
    timeout_us: u32,
) {
    request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
    request.params.earliest = raw::mpsl_timeslot_request_earliest_t {
        hfclk: TIMESLOT_HFCLK,
        priority,
        length_us,
        timeout_us,
    };
}

fn configure_normal_request(
    request: &mut raw::mpsl_timeslot_request_t,
    priority: u8,
    distance_us: u32,
    length_us: u32,
) {
    request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_NORMAL as u8;
    request.params.normal = raw::mpsl_timeslot_request_normal_t {
        hfclk: TIMESLOT_HFCLK,
        priority,
        distance_us,
        length_us,
    };
}

struct Timer0RawMutex;
unsafe impl RawMutex for Timer0RawMutex {
    const INIT: Self = Timer0RawMutex;
    fn lock<R>(&self, f: impl FnOnce() -> R) -> R {
        unsafe {
            let nvic = &*NVIC::PTR;
            let irq = TIMESLOT_TIMER_INTERRUPT as usize;
            nvic.icer[irq / 32].write(1u32 << (irq % 32));
            compiler_fence(Ordering::SeqCst);
            let r = f();
            compiler_fence(Ordering::SeqCst);
            nvic.iser[irq / 32].write(1u32 << (irq % 32));
            r
        }
    }
}

/// Cumulative signal counts across a timeslot session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SignalCounters {
    pub start: u32,
    pub timer0: u32,
    pub radio: u32,
    pub blocked: u32,
    pub cancelled: u32,
    pub session_idle: u32,
    pub session_closed: u32,
    pub overstayed: u32,
    pub invalid_return: u32,
    pub extend_failed: u32,
    pub extend_succeeded: u32,
    pub radio_disable_timeout: u32,
}

impl SignalCounters {
    pub const ZERO: Self = Self {
        start: 0,
        timer0: 0,
        radio: 0,
        blocked: 0,
        cancelled: 0,
        session_idle: 0,
        session_closed: 0,
        overstayed: 0,
        invalid_return: 0,
        extend_failed: 0,
        extend_succeeded: 0,
        radio_disable_timeout: 0,
    };

    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            start: self.start.saturating_add(other.start),
            timer0: self.timer0.saturating_add(other.timer0),
            radio: self.radio.saturating_add(other.radio),
            blocked: self.blocked.saturating_add(other.blocked),
            cancelled: self.cancelled.saturating_add(other.cancelled),
            session_idle: self.session_idle.saturating_add(other.session_idle),
            session_closed: self.session_closed.saturating_add(other.session_closed),
            overstayed: self.overstayed.saturating_add(other.overstayed),
            invalid_return: self.invalid_return.saturating_add(other.invalid_return),
            extend_failed: self.extend_failed.saturating_add(other.extend_failed),
            extend_succeeded: self.extend_succeeded.saturating_add(other.extend_succeeded),
            radio_disable_timeout: self
                .radio_disable_timeout
                .saturating_add(other.radio_disable_timeout),
        }
    }

    fn saturating_sub(self, previous: Self) -> Self {
        Self {
            start: self.start.saturating_sub(previous.start),
            timer0: self.timer0.saturating_sub(previous.timer0),
            radio: self.radio.saturating_sub(previous.radio),
            blocked: self.blocked.saturating_sub(previous.blocked),
            cancelled: self.cancelled.saturating_sub(previous.cancelled),
            session_idle: self.session_idle.saturating_sub(previous.session_idle),
            session_closed: self.session_closed.saturating_sub(previous.session_closed),
            overstayed: self.overstayed.saturating_sub(previous.overstayed),
            invalid_return: self.invalid_return.saturating_sub(previous.invalid_return),
            extend_failed: self.extend_failed.saturating_sub(previous.extend_failed),
            extend_succeeded: self
                .extend_succeeded
                .saturating_sub(previous.extend_succeeded),
            radio_disable_timeout: self
                .radio_disable_timeout
                .saturating_sub(previous.radio_disable_timeout),
        }
    }
}

struct InnerState {
    counters: SignalCounters,
    done: bool,
    waker: WakerRegistration,
    request: raw::mpsl_timeslot_request_t,
    return_param: raw::mpsl_timeslot_signal_return_param_t,
    in_slot_match_us: u32,
    /// Chaining: target number of SIGNAL_START events. 0 = single-shot (no chaining).
    target_count: u32,
}

unsafe impl Send for InnerState {}
unsafe impl Sync for InnerState {}

struct State {
    busy: AtomicBool,
    inner: Mutex<Timer0RawMutex, RefCell<InnerState>>,
}

static STATE: State = State::new();

impl State {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            inner: Mutex::new(RefCell::new(InnerState {
                counters: SignalCounters::ZERO,
                done: false,
                waker: WakerRegistration::new(),
                request: raw::mpsl_timeslot_request_t {
                    request_type: raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8,
                    params: raw::mpsl_timeslot_request_t__bindgen_ty_1 {
                        earliest: raw::mpsl_timeslot_request_earliest_t {
                            hfclk: raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8,
                            priority: raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8,
                            length_us: 0,
                            timeout_us: 1_000_000,
                        },
                    },
                },
                return_param: raw::mpsl_timeslot_signal_return_param_t {
                    callback_action: 0,
                    params: raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1 {
                        request:
                            raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1__bindgen_ty_1 {
                                p_next: core::ptr::null_mut(),
                            },
                    },
                },
                in_slot_match_us: 4500,
                target_count: 0,
            })),
        }
    }

    fn with_inner<F: FnOnce(&mut InnerState) -> R, R>(&self, f: F) -> R {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            f(&mut inner)
        })
    }

    fn try_enter(&'static self) -> Result<BusyGuard, Error> {
        BusyGuard::new(&self.busy)
    }
}

struct BusyGuard {
    busy: &'static AtomicBool,
}

impl BusyGuard {
    fn new(busy: &'static AtomicBool) -> Result<Self, Error> {
        match busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(Self { busy }),
            Err(_) => Err(Error::Busy),
        }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn timeslot_callback(
    session_id: u8,
    signal: u32,
) -> *mut raw::mpsl_timeslot_signal_return_param_t {
    match signal {
        raw::MPSL_TIMESLOT_SIGNAL_START => STATE.with_inner(|state| {
            state.counters.start += 1;

            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.cc(0).write_value(state.in_slot_match_us);
            t.intenset().write(|w| w.set_compare(0, true));

            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_TIMER0 => STATE.with_inner(|state| {
            state.counters.timer0 += 1;

            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.intenclr().write(|w| w.set_compare(0, true));

            let chain = state.target_count > 0 && state.counters.start < state.target_count;
            if chain {
                // Chain next slot via ACTION_REQUEST.
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next = core::ptr::from_mut(&mut state.request);
            } else {
                state.done = true;
                state.waker.wake();
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            }
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_RADIO => STATE.with_inner(|state| {
            state.counters.radio += 1;
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_IDLE => STATE.with_inner(|state| {
            state.counters.session_idle += 1;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_BLOCKED | raw::MPSL_TIMESLOT_SIGNAL_CANCELLED => {
            let request = STATE.with_inner(|state| {
                if signal == raw::MPSL_TIMESLOT_SIGNAL_BLOCKED {
                    state.counters.blocked += 1;
                } else {
                    state.counters.cancelled += 1;
                }
                state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_HIGH as u8;
                state.request.params.earliest.timeout_us =
                    raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US;
                core::ptr::from_ref(&state.request)
            });
            let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
            if ret < 0 {
                STATE.with_inner(|state| {
                    state.counters.invalid_return += 1;
                    state.done = true;
                    state.waker.wake();
                });
            }
            core::ptr::null_mut()
        }

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_CLOSED => STATE.with_inner(|state| {
            state.counters.session_closed += 1;
            state.done = true;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_OVERSTAYED => STATE.with_inner(|state| {
            state.counters.overstayed += 1;
            state.done = true;
            state.waker.wake();
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_INVALID_RETURN => STATE.with_inner(|state| {
            state.counters.invalid_return += 1;
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_EXTEND_FAILED => STATE.with_inner(|state| {
            state.counters.extend_failed += 1;
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_EXTEND_SUCCEEDED => STATE.with_inner(|state| {
            state.counters.extend_succeeded += 1;
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        _ => STATE.with_inner(|state| {
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            &mut state.return_param as *mut _
        }),
    }
}

struct OnDrop<F: FnOnce()> {
    f: core::mem::MaybeUninit<F>,
}

impl<F: FnOnce()> OnDrop<F> {
    fn new(f: F) -> Self {
        Self {
            f: core::mem::MaybeUninit::new(f),
        }
    }
    fn defuse(self) {
        core::mem::forget(self);
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        unsafe { self.f.as_ptr().read()() };
    }
}

/// Request a single timeslot and wait for completion.
///
/// Opens a session, requests one EARLIEST timeslot of `slot_length_us` µs,
/// arms TIMER0 CC[0] at `in_slot_match_us` µs from slot start. When the
/// TIMER0 compare fires the slot is ended. Returns signal counters for
/// diagnostics.
pub async fn run_single_slot(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    slot_length_us: u32,
    in_slot_match_us: u32,
) -> Result<SignalCounters, Error> {
    let _busy = STATE.try_enter()?;

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(timeslot_callback), (&mut session_id) as *mut _)
    };
    mpsl_ok(ret)?;

    let _drop = OnDrop::new(|| {
        let _ = unsafe { raw::mpsl_timeslot_session_close(session_id) };
    });

    STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = 0; // single-shot
        state.in_slot_match_us = in_slot_match_us;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
    });

    let request = STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    mpsl_ok(ret)?;

    poll_fn(|cx| {
        STATE.with_inner(|state| {
            state.waker.register(cx.waker());
            if state.done {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    })
    .await;

    _drop.defuse();
    unsafe {
        let ret = raw::mpsl_timeslot_session_close(session_id);
        mpsl_ok(ret)?;
    }

    Ok(STATE.with_inner(|state| state.counters))
}

/// Request `count` chained timeslots in a single session.
///
/// Opens a session, requests the first EARLIEST timeslot, then chains each
/// subsequent slot via `ACTION_REQUEST` from the `SIGNAL_TIMER0` callback.
/// After all `count` slots complete, returns cumulative signal counters.
///
/// BLOCKED/CANCELLED are handled by upgrading to HIGH priority and retrying.
pub async fn run_chained_slots(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    slot_length_us: u32,
    in_slot_match_us: u32,
    count: u32,
) -> Result<SignalCounters, Error> {
    let _busy = STATE.try_enter()?;

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(timeslot_callback), (&mut session_id) as *mut _)
    };
    mpsl_ok(ret)?;

    let _drop = OnDrop::new(|| {
        let _ = unsafe { raw::mpsl_timeslot_session_close(session_id) };
    });

    STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = count;
        state.in_slot_match_us = in_slot_match_us;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
    });

    let request = STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    mpsl_ok(ret)?;

    poll_fn(|cx| {
        STATE.with_inner(|state| {
            state.waker.register(cx.waker());
            if state.done {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    })
    .await;

    _drop.defuse();
    unsafe {
        let ret = raw::mpsl_timeslot_session_close(session_id);
        mpsl_ok(ret)?;
    }

    Ok(STATE.with_inner(|state| state.counters))
}

// ---- PTX-in-timeslot ----

use crate::addresses::EsbAddresses;
use crate::config::EsbConfig;
use crate::header::EsbHeader;
use crate::radio::EsbRadio;

use core::cell::UnsafeCell;

/// Result of a PTX timeslot session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PtxSlotResult {
    pub counters: SignalCounters,
    pub tx_count: u32,
    pub ack_ok_count: u32,
    pub ack_payload_count: u32,
    pub ack_inversions: u32,
    pub last_ack_counter: u32,
}

/// Phase within a single PTX timeslot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtxPhase {
    Idle,
    Tx,
    WaitAck,
    Done,
}

struct PtxInnerState {
    counters: SignalCounters,
    done: bool,
    waker: WakerRegistration,
    request: raw::mpsl_timeslot_request_t,
    return_param: raw::mpsl_timeslot_signal_return_param_t,
    slot_length_us: u32,
    in_slot_match_us: u32,
    request_timeout_us: u32,
    target_count: u32,
    config: Option<crate::config::EsbConfig>,
    addresses: Option<crate::addresses::EsbAddresses>,
    pid: u8,
    phase: PtxPhase,
    tx_count: u32,
    ack_ok_count: u32,
    tx_pipe: u8,
    payload_byte: u8,
    packets_per_slot: u32,
    packets_sent_this_slot: u32,
    ack_payload_count: u32,
    ack_inversions: u32,
    last_ack_counter: u32,
    schedule_hint_count: u32,
    schedule_hint_bad_count: u32,
    last_schedule_window_id: u32,
    schedule_tracker: ScheduleTracker,
    schedule_gate: PtxScheduleGateConfig,
    schedule_skip_slots_remaining: u8,
    schedule_skip_count: u32,
    schedule_lock_active: bool,
    schedule_lock_miss_streak: u8,
    schedule_lock_count: u32,
    schedule_reacquire_count: u32,
    schedule_period_us: u32,
    schedule_next_distance_us: u32,
    poll_pipes: u8,
    poll_pipe_mask: u8,
    poll_report_every: u32,
    poll_slots_since_report: u32,
    poll_report_ready: bool,
    ack_ok_per_pipe: [u32; NUM_PIPES],
    tx_per_pipe: [u32; NUM_PIPES],
    retry_count: u8,
    max_retries: u8,
    acked_this_slot: bool,
    ack_timeout_us: u32,
    ack_timeout_count: u32,
    ack_crc_fail_count: u32,
    ack_timeout_per_pipe: [u32; NUM_PIPES],
    ack_crc_fail_per_pipe: [u32; NUM_PIPES],
    event_mode: bool,
    event_pending: bool,
    event_result_ready: bool,
    event_ack_ok: bool,
}

unsafe impl Send for PtxInnerState {}
unsafe impl Sync for PtxInnerState {}

struct PtxState {
    busy: AtomicBool,
    inner: Mutex<Timer0RawMutex, RefCell<PtxInnerState>>,
}

static PTX_STATE: PtxState = PtxState::new();

#[repr(C, align(4))]
struct PtxBuffers {
    tx: UnsafeCell<[u8; 256]>,
    rx: UnsafeCell<[u8; 256]>,
}
unsafe impl Sync for PtxBuffers {}

#[unsafe(link_section = ".data")]
static PTX_BUFS: PtxBuffers = PtxBuffers {
    tx: UnsafeCell::new([0u8; 256]),
    rx: UnsafeCell::new([0u8; 256]),
};

fn ptx_set_earliest_request(state: &mut PtxInnerState, priority: u8, timeout_us: u32) {
    configure_earliest_request(
        &mut state.request,
        priority,
        state.slot_length_us,
        timeout_us,
    );
}

fn ptx_set_normal_request(state: &mut PtxInnerState, distance_us: u32) {
    configure_normal_request(
        &mut state.request,
        TIMESLOT_PRIORITY_NORMAL,
        distance_us,
        state.slot_length_us,
    );
}

fn ptx_configure_next_poll_request(state: &mut PtxInnerState) {
    if state.schedule_gate.mode == PtxScheduleMode::PhaseLocked && state.schedule_lock_active {
        let distance_us = if state.schedule_next_distance_us > 0 {
            let distance = state.schedule_next_distance_us;
            state.schedule_next_distance_us = 0;
            distance
        } else {
            state.schedule_period_us
        };

        if distance_us > 0 && distance_us <= raw::MPSL_TIMESLOT_DISTANCE_MAX_US {
            ptx_set_normal_request(state, distance_us);
            return;
        }

        state.schedule_lock_active = false;
        state.schedule_lock_miss_streak = 0;
        state.schedule_reacquire_count += 1;
    }

    ptx_set_earliest_request(state, TIMESLOT_PRIORITY_NORMAL, state.request_timeout_us);
}

fn ptx_observe_schedule_hint(state: &mut PtxInnerState, hint: ScheduleHint, ack_offset_us: u32) {
    state.schedule_hint_count += 1;
    state.last_schedule_window_id = hint.window_id;
    state.schedule_tracker.observe_hint(hint);

    match state.schedule_gate.mode {
        PtxScheduleMode::Disabled => {}
        PtxScheduleMode::FixedSkipAfterHint => {
            state.schedule_skip_slots_remaining = state.schedule_gate.skip_after_hint_slots;
        }
        PtxScheduleMode::PhaseLocked => {
            if !state.schedule_lock_active {
                state.schedule_lock_count += 1;
            }
            state.schedule_lock_active = true;
            state.schedule_lock_miss_streak = 0;
            state.schedule_period_us = hint.period_us;

            let min_distance = state.in_slot_match_us.saturating_add(100);
            let hinted_distance = ack_offset_us
                .saturating_add(hint.next_delay_us)
                .saturating_add(state.schedule_gate.lock_tx_offset_us);
            state.schedule_next_distance_us = hinted_distance.max(min_distance);
        }
    }
}

fn ptx_observe_schedule_miss(state: &mut PtxInnerState) {
    state.schedule_tracker.observe_miss();

    if state.schedule_gate.mode == PtxScheduleMode::PhaseLocked && state.schedule_lock_active {
        state.schedule_lock_miss_streak = state.schedule_lock_miss_streak.saturating_add(1);
        if state.schedule_lock_miss_streak >= state.schedule_gate.lock_miss_limit {
            state.schedule_lock_active = false;
            state.schedule_lock_miss_streak = 0;
            state.schedule_next_distance_us = 0;
            state.schedule_reacquire_count += 1;
        }
    }
}

impl PtxState {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            inner: Mutex::new(RefCell::new(PtxInnerState {
                counters: SignalCounters::ZERO,
                done: false,
                waker: WakerRegistration::new(),
                request: raw::mpsl_timeslot_request_t {
                    request_type: raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8,
                    params: raw::mpsl_timeslot_request_t__bindgen_ty_1 {
                        earliest: raw::mpsl_timeslot_request_earliest_t {
                            hfclk: raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8,
                            priority: raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8,
                            length_us: 0,
                            timeout_us: 1_000_000,
                        },
                    },
                },
                return_param: raw::mpsl_timeslot_signal_return_param_t {
                    callback_action: 0,
                    params: raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1 {
                        request:
                            raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1__bindgen_ty_1 {
                                p_next: core::ptr::null_mut(),
                            },
                    },
                },
                slot_length_us: 0,
                in_slot_match_us: 5500,
                request_timeout_us: 1_000_000,
                target_count: 0,
                config: None,
                addresses: None,
                pid: 0,
                phase: PtxPhase::Idle,
                tx_count: 0,
                ack_ok_count: 0,
                tx_pipe: 0,
                payload_byte: 0,
                packets_per_slot: 1,
                packets_sent_this_slot: 0,
                ack_payload_count: 0,
                ack_inversions: 0,
                last_ack_counter: 0,
                schedule_hint_count: 0,
                schedule_hint_bad_count: 0,
                last_schedule_window_id: 0,
                schedule_tracker: ScheduleTracker::new(),
                schedule_gate: PtxScheduleGateConfig::disabled(),
                schedule_skip_slots_remaining: 0,
                schedule_skip_count: 0,
                schedule_lock_active: false,
                schedule_lock_miss_streak: 0,
                schedule_lock_count: 0,
                schedule_reacquire_count: 0,
                schedule_period_us: 0,
                schedule_next_distance_us: 0,
                poll_pipes: 0,
                poll_pipe_mask: 0,
                poll_report_every: 0,
                poll_slots_since_report: 0,
                poll_report_ready: false,
                ack_ok_per_pipe: [0; NUM_PIPES],
                tx_per_pipe: [0; NUM_PIPES],
                retry_count: 0,
                max_retries: 3,
                acked_this_slot: false,
                ack_timeout_us: 600,
                ack_timeout_count: 0,
                ack_crc_fail_count: 0,
                ack_timeout_per_pipe: [0; NUM_PIPES],
                ack_crc_fail_per_pipe: [0; NUM_PIPES],
                event_mode: false,
                event_pending: false,
                event_result_ready: false,
                event_ack_ok: false,
            })),
        }
    }

    fn with_inner<F: FnOnce(&mut PtxInnerState) -> R, R>(&self, f: F) -> R {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            f(&mut inner)
        })
    }

    fn try_enter(&'static self) -> Result<BusyGuard, Error> {
        BusyGuard::new(&self.busy)
    }
}

unsafe extern "C" fn ptx_timeslot_callback(
    session_id: u8,
    signal: u32,
) -> *mut raw::mpsl_timeslot_signal_return_param_t {
    match signal {
        raw::MPSL_TIMESLOT_SIGNAL_START => PTX_STATE.with_inner(|state| {
            state.counters.start += 1;

            if state.event_mode && !state.event_pending {
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
                return &mut state.return_param as *mut _;
            }

            state.phase = PtxPhase::Tx;

            let (Some(config), Some(addresses)) = (state.config.as_ref(), state.addresses.as_ref())
            else {
                state.counters.invalid_return += 1;
                state.done = true;
                state.waker.wake();
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
                return &mut state.return_param as *mut _;
            };

            // In poll mode, advance to next pipe in mask.
            if state.poll_pipes > 0 {
                if let Some(next) = next_pipe_in_mask(state.tx_pipe, state.poll_pipe_mask) {
                    state.tx_pipe = next;
                }
            }

            if state.schedule_gate.mode == PtxScheduleMode::FixedSkipAfterHint
                && state.schedule_skip_slots_remaining > 0
            {
                state.schedule_skip_slots_remaining -= 1;
                state.schedule_skip_count += 1;
                state.phase = PtxPhase::Done;

                let t = pac::TIMER0;
                t.events_compare(0).write_value(0);
                t.events_compare(1).write_value(0);
                t.cc(0).write_value(state.in_slot_match_us);
                t.cc(1).write_value(0xFFFFFFFF);
                t.intenset().write(|w| {
                    w.set_compare(0, true);
                    w.set_compare(1, false);
                });

                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
                return &mut state.return_param as *mut _;
            }

            // Power cycle RADIO.
            let r = pac::RADIO;
            r.power().write(|w| w.set_power(false));
            r.power().write(|w| w.set_power(true));

            // Full ESB register init.
            let mut radio = crate::radio::EsbRadio::new(pac::RADIO);
            radio.init(config, addresses);
            radio.restore_pid_state([state.pid; 8]);

            // Prepare TX buffer. Event mode keeps the payload staged by send().
            let tx_buf = unsafe { &mut *PTX_BUFS.tx.get() };
            let dma_off = crate::header::EsbHeader::DMA_OFFSET;
            if !state.event_mode {
                let counter = state.tx_count;
                write_counter_packet(tx_buf, state.pid, counter);
            }

            // Arm TIMER0 CC[0] for slot end, CC[1] for ACK timeout.
            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.events_compare(1).write_value(0);
            t.cc(0).write_value(state.in_slot_match_us);
            t.cc(1).write_value(0xFFFFFFFF);
            t.intenset().write(|w| {
                w.set_compare(0, true);
                w.set_compare(1, true);
            });

            // Trigger TX with ACK.
            let dma_ptr = unsafe { tx_buf.as_mut_ptr().add(dma_off) };
            radio.transmit(state.tx_pipe, dma_ptr, true);
            state.tx_count += 1;
            state.packets_sent_this_slot = 1;
            state.retry_count = 0;
            state.acked_this_slot = false;
            if state.poll_pipes > 0 {
                let pipe = state.tx_pipe as usize;
                if pipe < NUM_PIPES {
                    state.tx_per_pipe[pipe] += 1;
                }
            }

            // Ensure RADIO NVIC is unmasked so MPSL delivers SIGNAL_RADIO.
            unsafe {
                cortex_m::peripheral::NVIC::unmask(pac::Interrupt::RADIO);
            }

            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_RADIO => PTX_STATE.with_inner(|state| {
            state.counters.radio += 1;

            let r = pac::RADIO;
            let disabled = r.events_disabled().read() == 1;

            match state.phase {
                PtxPhase::Tx if disabled => {
                    r.events_disabled().write_value(0);

                    let rx_buf = unsafe { &mut *PTX_BUFS.rx.get() };
                    let dma_ptr = unsafe {
                        rx_buf
                            .as_mut_ptr()
                            .add(crate::header::EsbHeader::DMA_OFFSET)
                    };

                    r.events_ready().write_value(0);
                    compiler_fence(Ordering::Release);
                    r.packetptr().write_value(dma_ptr as u32);
                    r.shorts().modify(|w| w.set_disabled_rxen(false));

                    let t = pac::TIMER0;
                    t.tasks_capture(1).write_value(1);
                    let now = t.cc(1).read();
                    t.cc(1).write_value(now.wrapping_add(state.ack_timeout_us));
                    t.events_compare(1).write_value(0);
                    t.intenset().write(|w| w.set_compare(1, true));

                    state.phase = PtxPhase::WaitAck;
                }
                PtxPhase::WaitAck if disabled => {
                    r.events_disabled().write_value(0);

                    let crc_ok =
                        r.crcstatus().read().crcstatus() == pac::radio::vals::Crcstatus::CRCOK;
                    compiler_fence(Ordering::Acquire);

                    if crc_ok {
                        let t = pac::TIMER0;
                        t.intenclr().write(|w| w.set_compare(1, true));
                        t.events_compare(1).write_value(0);

                        state.ack_ok_count += 1;
                        state.acked_this_slot = true;
                        if state.poll_pipes > 0 {
                            let pipe = state.tx_pipe as usize;
                            if pipe < NUM_PIPES {
                                state.ack_ok_per_pipe[pipe] += 1;
                            }
                        }

                        let rx_buf = unsafe { &*PTX_BUFS.rx.get() };
                        if let Some(counter) = read_counter_payload(rx_buf) {
                            state.ack_payload_count += 1;
                            if counter < state.last_ack_counter {
                                state.ack_inversions += 1;
                            }
                            state.last_ack_counter = counter;

                            let dma = crate::header::EsbHeader::DMA_OFFSET;
                            let payload = crate::header::EsbHeader::PAYLOAD_OFFSET;
                            let len = rx_buf[dma] as usize;
                            let end = payload.saturating_add(len);
                            if end <= rx_buf.len() {
                                let t = pac::TIMER0;
                                t.tasks_capture(2).write_value(1);
                                let ack_offset_us = t.cc(2).read();
                                match decode_counter_payload_schedule_hint(&rx_buf[payload..end]) {
                                    Ok(hint) => {
                                        ptx_observe_schedule_hint(state, hint, ack_offset_us)
                                    }
                                    Err(_) => {
                                        state.schedule_hint_bad_count += 1;
                                        state.schedule_tracker.observe_bad_hint();
                                    }
                                }
                            } else {
                                state.schedule_hint_bad_count += 1;
                                state.schedule_tracker.observe_bad_hint();
                            }
                        } else {
                            state.schedule_hint_bad_count += 1;
                            state.schedule_tracker.observe_bad_hint();
                        }
                    }

                    if !crc_ok {
                        state.ack_crc_fail_count += 1;
                        ptx_observe_schedule_miss(state);
                        let pipe = state.tx_pipe as usize;
                        if pipe < NUM_PIPES {
                            state.ack_crc_fail_per_pipe[pipe] += 1;
                        }
                    }

                    if state.acked_this_slot {
                        state.pid = advance_pid(state.pid);
                        state.payload_byte = state.payload_byte.wrapping_add(1);
                    }
                    if disable_radio_bounded(false, false).timed_out() {
                        state.counters.radio_disable_timeout += 1;
                    }

                    state.phase = PtxPhase::Done;
                }
                _ => {}
            }

            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_TIMER0 => PTX_STATE.with_inner(|state| {
            state.counters.timer0 += 1;

            let t = pac::TIMER0;

            // Check CC[1] first: ACK timeout → retry.
            if t.events_compare(1).read() == 1 && state.phase == PtxPhase::WaitAck {
                t.events_compare(1).write_value(0);
                state.ack_timeout_count += 1;
                ptx_observe_schedule_miss(state);
                let pipe = state.tx_pipe as usize;
                if pipe < NUM_PIPES {
                    state.ack_timeout_per_pipe[pipe] += 1;
                }

                let r = pac::RADIO;
                if disable_radio_bounded(true, false).timed_out() {
                    state.counters.radio_disable_timeout += 1;
                }

                if state.retry_count < state.max_retries {
                    state.retry_count += 1;
                    state.phase = PtxPhase::Tx;

                    let tx_buf = unsafe { &mut *PTX_BUFS.tx.get() };
                    let dma_off = crate::header::EsbHeader::DMA_OFFSET;
                    if !state.event_mode {
                        write_counter_packet(tx_buf, state.pid, state.tx_count.wrapping_sub(1));
                    }

                    r.shorts().modify(|w| w.set_disabled_rxen(true));
                    r.intenset().write(|w| w.set_disabled(true));
                    r.txaddress().write(|w| w.set_txaddress(state.tx_pipe));
                    r.rxaddresses()
                        .write_value(pac::radio::regs::Rxaddresses(1 << state.tx_pipe));
                    let dma_ptr = unsafe { tx_buf.as_mut_ptr().add(dma_off) };
                    r.packetptr().write_value(dma_ptr as u32);
                    r.events_address().write_value(0);
                    r.events_disabled().write_value(0);
                    r.events_ready().write_value(0);
                    r.events_end().write_value(0);
                    r.events_payload().write_value(0);
                    compiler_fence(Ordering::Release);
                    r.tasks_txen().write_value(1);

                    t.tasks_capture(1).write_value(1);
                    let now = t.cc(1).read();
                    t.cc(1).write_value(now.wrapping_add(state.ack_timeout_us));
                    t.events_compare(1).write_value(0);
                    t.intenset().write(|w| w.set_compare(1, true));

                    state.return_param.callback_action =
                        raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
                    return &mut state.return_param as *mut _;
                } else {
                    state.phase = PtxPhase::Done;
                    t.intenclr().write(|w| w.set_compare(1, true));
                }
            }

            // CC[0]: slot end.
            if t.events_compare(0).read() == 1 {
                t.events_compare(0).write_value(0);
                t.intenclr().write(|w| {
                    w.set_compare(0, true);
                    w.set_compare(1, true);
                });

                if state.phase != PtxPhase::Done && state.phase != PtxPhase::Idle {
                    if disable_radio_bounded(true, true).timed_out() {
                        state.counters.radio_disable_timeout += 1;
                    }
                    state.phase = PtxPhase::Done;
                }

                let poll_active = state.poll_pipes > 0;
                let event_active = state.event_mode;

                if event_active {
                    state.event_result_ready = true;
                    state.event_pending = false;
                    state.event_ack_ok = state.acked_this_slot;
                    state.waker.wake();
                    state.return_param.callback_action =
                        raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
                } else if poll_active {
                    state.poll_slots_since_report += 1;
                    if state.poll_slots_since_report >= state.poll_report_every {
                        state.poll_slots_since_report = 0;
                        state.poll_report_ready = true;
                        state.waker.wake();
                    }

                    ptx_configure_next_poll_request(state);
                    state.return_param.callback_action =
                        raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                    state.return_param.params.request.p_next =
                        core::ptr::from_mut(&mut state.request);
                } else {
                    let chain = state.target_count > 0 && state.counters.start < state.target_count;
                    if chain {
                        state.return_param.callback_action =
                            raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                        state.return_param.params.request.p_next =
                            core::ptr::from_mut(&mut state.request);
                    } else {
                        state.done = true;
                        state.waker.wake();
                        state.return_param.callback_action =
                            raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
                    }
                }
            }

            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_IDLE => {
            let request = PTX_STATE.with_inner(|state| {
                state.counters.session_idle += 1;
                if state.poll_pipes > 0 {
                    if state.schedule_gate.mode == PtxScheduleMode::PhaseLocked {
                        state.schedule_lock_active = false;
                        state.schedule_lock_miss_streak = 0;
                        state.schedule_next_distance_us = 0;
                        state.schedule_reacquire_count += 1;
                    }
                    ptx_set_earliest_request(
                        state,
                        TIMESLOT_PRIORITY_NORMAL,
                        state.request_timeout_us,
                    );
                    state.poll_report_ready = true;
                    state.waker.wake();
                    Some(core::ptr::from_ref(&state.request))
                } else {
                    state.waker.wake();
                    None
                }
            });

            if let Some(request) = request {
                let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
                if ret < 0 {
                    PTX_STATE.with_inner(|state| {
                        state.counters.invalid_return += 1;
                        state.done = true;
                        state.waker.wake();
                    });
                }
            }

            core::ptr::null_mut()
        }

        raw::MPSL_TIMESLOT_SIGNAL_BLOCKED | raw::MPSL_TIMESLOT_SIGNAL_CANCELLED => {
            let request = PTX_STATE.with_inner(|state| {
                if signal == raw::MPSL_TIMESLOT_SIGNAL_BLOCKED {
                    state.counters.blocked += 1;
                } else {
                    state.counters.cancelled += 1;
                }
                if state.schedule_gate.mode == PtxScheduleMode::PhaseLocked
                    && state.schedule_lock_active
                {
                    state.schedule_lock_active = false;
                    state.schedule_lock_miss_streak = 0;
                    state.schedule_next_distance_us = 0;
                    state.schedule_reacquire_count += 1;
                }
                ptx_set_earliest_request(
                    state,
                    TIMESLOT_PRIORITY_HIGH,
                    raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US,
                );
                core::ptr::from_ref(&state.request)
            });
            let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
            if ret < 0 {
                PTX_STATE.with_inner(|state| {
                    state.counters.invalid_return += 1;
                    state.done = true;
                    state.waker.wake();
                });
            }
            core::ptr::null_mut()
        }

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_CLOSED => PTX_STATE.with_inner(|state| {
            state.counters.session_closed += 1;
            state.done = true;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_OVERSTAYED => {
            let ptr = PTX_STATE.with_inner(|state| {
                state.counters.overstayed += 1;
                state.phase = PtxPhase::Idle;
                if state.poll_pipes > 0 {
                    state.schedule_lock_active = false;
                    state.schedule_lock_miss_streak = 0;
                    state.schedule_next_distance_us = 0;
                    state.schedule_reacquire_count += 1;
                    ptx_set_earliest_request(
                        state,
                        TIMESLOT_PRIORITY_HIGH,
                        raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US,
                    );
                    state.return_param.callback_action =
                        raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                    state.return_param.params.request.p_next =
                        core::ptr::from_mut(&mut state.request);
                } else {
                    if state.event_mode {
                        state.event_result_ready = true;
                        state.event_pending = false;
                        state.event_ack_ok = false;
                        state.waker.wake();
                        state.return_param.callback_action =
                            raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
                    } else {
                        state.done = true;
                        state.waker.wake();
                        state.return_param.callback_action =
                            raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
                    }
                }
                &mut state.return_param as *mut _
            });
            ptr
        }

        _ => PTX_STATE.with_inner(|state| {
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            &mut state.return_param as *mut _
        }),
    }
}

/// Run PTX transmissions inside chained timeslots.
///
/// Each timeslot sends exactly 1 ACK packet with an incrementing payload.
/// After `count` slots, returns cumulative results including ACK stats.
/// PID is saved/restored across slot boundaries.
pub async fn run_ptx_slots(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    config: &crate::config::EsbConfig,
    addresses: &crate::addresses::EsbAddresses,
    slot_length_us: u32,
    in_slot_match_us: u32,
    count: u32,
    tx_pipe: u8,
    packets_per_slot: u32,
) -> Result<PtxSlotResult, Error> {
    let _busy = PTX_STATE.try_enter()?;
    if config.payload_length < 4 {
        return Err(Error::InvalidParam);
    }

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(ptx_timeslot_callback), (&mut session_id) as *mut _)
    };
    mpsl_ok(ret)?;

    let _drop = OnDrop::new(|| {
        let _ = unsafe { raw::mpsl_timeslot_session_close(session_id) };
    });

    PTX_STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = count;
        state.slot_length_us = slot_length_us;
        state.in_slot_match_us = in_slot_match_us;
        state.request_timeout_us = 1_000_000;
        state.config = Some(config.clone());
        state.addresses = Some(addresses.clone());
        state.phase = PtxPhase::Idle;
        state.tx_count = 0;
        state.ack_ok_count = 0;
        state.ack_payload_count = 0;
        state.ack_inversions = 0;
        state.last_ack_counter = 0;
        state.schedule_hint_count = 0;
        state.schedule_hint_bad_count = 0;
        state.last_schedule_window_id = 0;
        state.schedule_tracker.reset();
        state.schedule_gate = PtxScheduleGateConfig::disabled();
        state.schedule_skip_slots_remaining = 0;
        state.schedule_skip_count = 0;
        state.schedule_lock_active = false;
        state.schedule_lock_miss_streak = 0;
        state.schedule_lock_count = 0;
        state.schedule_reacquire_count = 0;
        state.schedule_period_us = 0;
        state.schedule_next_distance_us = 0;
        state.tx_pipe = tx_pipe;
        state.payload_byte = 0;
        state.packets_per_slot = packets_per_slot;
        state.packets_sent_this_slot = 0;
        ptx_set_earliest_request(state, TIMESLOT_PRIORITY_NORMAL, 1_000_000);
    });

    let request = PTX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    mpsl_ok(ret)?;

    poll_fn(|cx| {
        PTX_STATE.with_inner(|state| {
            state.waker.register(cx.waker());
            if state.done {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    })
    .await;

    _drop.defuse();
    unsafe {
        let ret = raw::mpsl_timeslot_session_close(session_id);
        mpsl_ok(ret)?;
    }

    Ok(PTX_STATE.with_inner(|state| PtxSlotResult {
        counters: state.counters,
        tx_count: state.tx_count,
        ack_ok_count: state.ack_ok_count,
        ack_payload_count: state.ack_payload_count,
        ack_inversions: state.ack_inversions,
        last_ack_counter: state.last_ack_counter,
    }))
}

// ---- PRX-in-timeslot ----

/// Result of a PRX timeslot session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PrxSlotResult {
    pub counters: SignalCounters,
    pub rx_count: u32,
    pub dup_count: u32,
    pub bad_crc_count: u32,
    pub rx_per_pipe: [u32; NUM_PIPES],
    pub dup_per_pipe: [u32; NUM_PIPES],
    pub bad_crc_per_pipe: [u32; NUM_PIPES],
    pub ack_tx_per_pipe: [u32; NUM_PIPES],
    pub slot_length_us: u32,
    pub in_slot_match_us: u32,
    pub report_every: u32,
    pub phase: u8,
    pub slot_active: bool,
    pub last_request_kind: u8,
    pub normal_blocked: u32,
    pub earliest_blocked: u32,
    pub normal_cancelled: u32,
    pub earliest_cancelled: u32,
}

const PRX_REQ_EARLIEST: u8 = 1;
const PRX_REQ_NORMAL: u8 = 2;

/// Phase within a single PRX timeslot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrxPhase {
    Idle,
    Receiving,
    TxAck,
    TxRepeatedAck,
}

impl PrxPhase {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Receiving => 1,
            Self::TxAck => 2,
            Self::TxRepeatedAck => 3,
        }
    }
}

struct PrxInnerState {
    counters: SignalCounters,
    done: bool,
    waker: WakerRegistration,
    request: raw::mpsl_timeslot_request_t,
    return_param: raw::mpsl_timeslot_signal_return_param_t,
    slot_length_us: u32,
    in_slot_match_us: u32,
    request_timeout_us: u32,
    target_count: u32,
    config: Option<EsbConfig>,
    addresses: Option<EsbAddresses>,
    phase: PrxPhase,
    rx_count: u32,
    dup_count: u32,
    bad_crc_count: u32,
    last_pid: [u8; NUM_PIPES],
    last_crc: [u16; NUM_PIPES],
    last_valid: [bool; NUM_PIPES],
    enabled_pipes: u8,
    recovery_policy: RadioRecoveryPolicy,
    slot_active: bool,
    ack_counter: [u32; NUM_PIPES],
    rx_per_pipe: [u32; NUM_PIPES],
    dup_per_pipe: [u32; NUM_PIPES],
    bad_crc_per_pipe: [u32; NUM_PIPES],
    ack_tx_per_pipe: [u32; NUM_PIPES],
    report_every: u32,
    last_report_start: u32,
    report_ready: bool,
    prx_schedule: PrxScheduleConfig,
    prx_windows_since_gap: u32,
    retry_blocked_at_high_priority: bool,
    last_request_kind: u8,
    normal_blocked: u32,
    earliest_blocked: u32,
    normal_cancelled: u32,
    earliest_cancelled: u32,
}

unsafe impl Send for PrxInnerState {}
unsafe impl Sync for PrxInnerState {}

struct PrxState {
    busy: AtomicBool,
    inner: Mutex<Timer0RawMutex, RefCell<PrxInnerState>>,
}

static PRX_STATE: PrxState = PrxState::new();

#[repr(C, align(4))]
struct PrxBuffers {
    rx: UnsafeCell<[u8; 256]>,
    ack_tx: UnsafeCell<[u8; 256]>,
}
unsafe impl Sync for PrxBuffers {}

#[unsafe(link_section = ".data")]
static PRX_BUFS: PrxBuffers = PrxBuffers {
    rx: UnsafeCell::new([0u8; 256]),
    ack_tx: UnsafeCell::new([0u8; 256]),
};

fn prx_set_earliest_request(state: &mut PrxInnerState, priority: u8, timeout_us: u32) {
    configure_earliest_request(
        &mut state.request,
        priority,
        state.slot_length_us,
        timeout_us,
    );
    state.last_request_kind = PRX_REQ_EARLIEST;
}

fn prx_set_normal_request(state: &mut PrxInnerState, distance_us: u32) {
    configure_normal_request(
        &mut state.request,
        TIMESLOT_PRIORITY_NORMAL,
        distance_us,
        state.slot_length_us,
    );
    state.last_request_kind = PRX_REQ_NORMAL;
}

fn prx_next_normal_distance_us(state: &mut PrxInnerState) -> u32 {
    state.prx_windows_since_gap = state.prx_windows_since_gap.saturating_add(1);

    if state.prx_schedule.gap_after_windows > 0
        && state.prx_windows_since_gap >= state.prx_schedule.gap_after_windows
    {
        state.prx_windows_since_gap = 0;
        return state.prx_schedule.gap_distance_us;
    }

    if state.prx_schedule.normal_distance_us > 0 {
        state.prx_schedule.normal_distance_us
    } else {
        state.slot_length_us
    }
}

fn prx_next_window_delay_us(state: &PrxInnerState) -> u32 {
    let t = pac::TIMER0;
    t.tasks_capture(2).write_value(1);
    let elapsed_us = t.cc(2).read();
    state.slot_length_us.saturating_sub(elapsed_us)
}

fn write_ack_counter_packet(
    buf: &mut [u8; 256],
    counter: u32,
    window_id: u32,
    next_delay_us: u32,
    period_us: u32,
    window_us: u32,
) {
    let hint = ScheduleHint::new(0, window_id, next_delay_us, period_us, window_us);
    write_counter_schedule_packet(buf, 0, counter, hint);
}

impl PrxState {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            inner: Mutex::new(RefCell::new(PrxInnerState {
                counters: SignalCounters::ZERO,
                done: false,
                waker: WakerRegistration::new(),
                request: raw::mpsl_timeslot_request_t {
                    request_type: raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8,
                    params: raw::mpsl_timeslot_request_t__bindgen_ty_1 {
                        earliest: raw::mpsl_timeslot_request_earliest_t {
                            hfclk: raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8,
                            priority: raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8,
                            length_us: 0,
                            timeout_us: 1_000_000,
                        },
                    },
                },
                return_param: raw::mpsl_timeslot_signal_return_param_t {
                    callback_action: 0,
                    params: raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1 {
                        request:
                            raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1__bindgen_ty_1 {
                                p_next: core::ptr::null_mut(),
                            },
                    },
                },
                slot_length_us: 0,
                in_slot_match_us: 5500,
                request_timeout_us: 1_000_000,
                target_count: 0,
                config: None,
                addresses: None,
                phase: PrxPhase::Idle,
                rx_count: 0,
                dup_count: 0,
                bad_crc_count: 0,
                last_pid: [0; NUM_PIPES],
                last_crc: [0; NUM_PIPES],
                last_valid: [false; NUM_PIPES],
                enabled_pipes: 0x01,
                recovery_policy: RadioRecoveryPolicy::ForceResetAfterBoundedDisable,
                slot_active: false,
                ack_counter: [0; NUM_PIPES],
                rx_per_pipe: [0; NUM_PIPES],
                dup_per_pipe: [0; NUM_PIPES],
                bad_crc_per_pipe: [0; NUM_PIPES],
                ack_tx_per_pipe: [0; NUM_PIPES],
                report_every: 0,
                last_report_start: 0,
                report_ready: false,
                prx_schedule: PrxScheduleConfig::continuous(),
                prx_windows_since_gap: 0,
                retry_blocked_at_high_priority: true,
                last_request_kind: PRX_REQ_EARLIEST,
                normal_blocked: 0,
                earliest_blocked: 0,
                normal_cancelled: 0,
                earliest_cancelled: 0,
            })),
        }
    }

    fn with_inner<F: FnOnce(&mut PrxInnerState) -> R, R>(&self, f: F) -> R {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            f(&mut inner)
        })
    }

    fn try_enter(&'static self) -> Result<BusyGuard, Error> {
        BusyGuard::new(&self.busy)
    }
}

unsafe extern "C" fn prx_timeslot_callback(
    session_id: u8,
    signal: u32,
) -> *mut raw::mpsl_timeslot_signal_return_param_t {
    match signal {
        raw::MPSL_TIMESLOT_SIGNAL_START => PRX_STATE.with_inner(|state| {
            state.counters.start += 1;
            state.slot_active = true;

            let (Some(config), Some(addresses)) = (state.config.as_ref(), state.addresses.as_ref())
            else {
                state.counters.invalid_return += 1;
                state.done = true;
                state.waker.wake();
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
                return &mut state.return_param as *mut _;
            };

            let r = pac::RADIO;
            r.power().write(|w| w.set_power(false));
            r.power().write(|w| w.set_power(true));

            let mut radio = EsbRadio::new(pac::RADIO);
            radio.init(config, addresses);
            radio.restore_pid_state(state.last_pid);
            radio.restore_crc_state(state.last_crc);
            radio.restore_detection_valid_state(state.last_valid);

            // Manual ACK lets us program TXADDRESS from RXMATCH before ACK TX.
            let rx_buf = unsafe { &mut *PRX_BUFS.rx.get() };
            let dma_ptr = unsafe { rx_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
            radio.start_receiving_manual_ack(state.enabled_pipes, dma_ptr);

            // Arm TIMER0 for slot end.
            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.cc(0).write_value(state.in_slot_match_us);
            t.intenset().write(|w| w.set_compare(0, true));

            state.phase = PrxPhase::Receiving;

            unsafe {
                cortex_m::peripheral::NVIC::unmask(pac::Interrupt::RADIO);
            }

            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_RADIO => PRX_STATE.with_inner(|state| {
            state.counters.radio += 1;

            let r = pac::RADIO;
            let disabled = r.events_disabled().read() == 1;

            match state.phase {
                PrxPhase::Receiving if disabled => {
                    r.events_disabled().write_value(0);
                    compiler_fence(Ordering::Acquire);

                    // Check CRC.
                    if r.crcstatus().read().crcstatus() == pac::radio::vals::Crcstatus::CRCERROR {
                        state.bad_crc_count += 1;
                        let pipe = r.rxmatch().read().rxmatch() as usize;
                        if pipe < NUM_PIPES {
                            state.bad_crc_per_pipe[pipe] += 1;
                        }
                        // Restart RX: stop radio, re-enable shortcuts, start RX again.
                        let mut radio = EsbRadio::new(pac::RADIO);
                        radio.stop();
                        let rx_buf = unsafe { &mut *PRX_BUFS.rx.get() };
                        let dma_ptr = unsafe { rx_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                        radio.start_receiving_manual_ack(state.enabled_pipes, dma_ptr);
                    } else {
                        // CRC OK — read metadata from DMA buffer.
                        let rx_buf = unsafe { &*PRX_BUFS.rx.get() };
                        let pid_no_ack = rx_buf[EsbHeader::DMA_OFFSET + 1];
                        let pid = (pid_no_ack >> 1) & 0x03;
                        let no_ack = (pid_no_ack & 0x01) != 0;
                        let pipe = r.rxmatch().read().rxmatch() as usize;
                        let crc = r.rxcrc().read().rxcrc() as u16;

                        let is_dup = pipe < NUM_PIPES
                            && state.last_valid[pipe]
                            && state.last_crc[pipe] == crc
                            && state.last_pid[pipe] == pid;

                        if is_dup {
                            state.dup_count += 1;
                            if pipe < NUM_PIPES {
                                state.dup_per_pipe[pipe] += 1;
                            }
                            if no_ack {
                                // Dup NoAck — stop TX ramp, restart RX.
                                let mut radio = EsbRadio::new(pac::RADIO);
                                radio.stop();
                                let rx_buf = unsafe { &mut *PRX_BUFS.rx.get() };
                                let dma_ptr =
                                    unsafe { rx_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                                radio.start_receiving_manual_ack(state.enabled_pipes, dma_ptr);
                            } else {
                                // Dup with ACK — re-send same counter (retransmit).
                                let counter = if pipe < NUM_PIPES {
                                    state.ack_counter[pipe]
                                } else {
                                    0
                                };
                                let ack_buf = unsafe { &mut *PRX_BUFS.ack_tx.get() };
                                let next_delay_us = prx_next_window_delay_us(state);
                                write_ack_counter_packet(
                                    ack_buf,
                                    counter,
                                    state.counters.start,
                                    next_delay_us,
                                    state.slot_length_us,
                                    state.in_slot_match_us,
                                );
                                let dma_ptr =
                                    unsafe { ack_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                                let mut radio = EsbRadio::new(pac::RADIO);
                                radio.transmit_ack_manual(pipe as u8, dma_ptr);
                                if pipe < NUM_PIPES {
                                    state.ack_tx_per_pipe[pipe] += 1;
                                }
                                state.phase = PrxPhase::TxRepeatedAck;
                            }
                        } else {
                            // New packet.
                            if pipe < NUM_PIPES {
                                state.last_pid[pipe] = pid;
                                state.last_crc[pipe] = crc;
                                state.last_valid[pipe] = true;
                                state.rx_per_pipe[pipe] += 1;
                            }
                            state.rx_count += 1;

                            if no_ack {
                                // NoAck — stop TX ramp, restart RX.
                                let mut radio = EsbRadio::new(pac::RADIO);
                                radio.stop();
                                let rx_buf = unsafe { &mut *PRX_BUFS.rx.get() };
                                let dma_ptr =
                                    unsafe { rx_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                                radio.start_receiving_manual_ack(state.enabled_pipes, dma_ptr);
                            } else {
                                // Need ACK — send monotonic counter as payload.
                                let counter = if pipe < NUM_PIPES {
                                    state.ack_counter[pipe] += 1;
                                    state.ack_counter[pipe]
                                } else {
                                    0
                                };
                                let ack_buf = unsafe { &mut *PRX_BUFS.ack_tx.get() };
                                let next_delay_us = prx_next_window_delay_us(state);
                                write_ack_counter_packet(
                                    ack_buf,
                                    counter,
                                    state.counters.start,
                                    next_delay_us,
                                    state.slot_length_us,
                                    state.in_slot_match_us,
                                );
                                let dma_ptr =
                                    unsafe { ack_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                                let mut radio = EsbRadio::new(pac::RADIO);
                                radio.transmit_ack_manual(pipe as u8, dma_ptr);
                                if pipe < NUM_PIPES {
                                    state.ack_tx_per_pipe[pipe] += 1;
                                }
                                state.phase = PrxPhase::TxAck;
                            }
                        }
                    }
                }

                PrxPhase::TxAck if disabled => {
                    r.events_disabled().write_value(0);
                    compiler_fence(Ordering::Acquire);

                    // ACK sent — restart RX.
                    let rx_buf = unsafe { &mut *PRX_BUFS.rx.get() };
                    let dma_ptr = unsafe { rx_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                    let mut radio = EsbRadio::new(pac::RADIO);
                    radio.start_receiving_manual_ack(state.enabled_pipes, dma_ptr);
                    state.phase = PrxPhase::Receiving;
                }

                PrxPhase::TxRepeatedAck if disabled => {
                    r.events_disabled().write_value(0);
                    compiler_fence(Ordering::Acquire);

                    // Repeated ACK sent — restart RX.
                    let rx_buf = unsafe { &mut *PRX_BUFS.rx.get() };
                    let dma_ptr = unsafe { rx_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                    let mut radio = EsbRadio::new(pac::RADIO);
                    radio.start_receiving_manual_ack(state.enabled_pipes, dma_ptr);
                    state.phase = PrxPhase::Receiving;
                }

                _ => {}
            }

            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_TIMER0 => PRX_STATE.with_inner(|state| {
            state.counters.timer0 += 1;
            state.slot_active = false;

            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.intenclr().write(|w| w.set_compare(0, true));

            if quiesce_radio_before_timeslot_end(state.recovery_policy).timed_out() {
                state.counters.radio_disable_timeout += 1;
            }
            state.phase = PrxPhase::Idle;

            let long_session = state.report_every > 0;
            let batch_complete = long_session
                && state.counters.start.saturating_sub(state.last_report_start)
                    >= state.report_every;

            if batch_complete {
                state.last_report_start = state.counters.start;
                state.report_ready = true;
                state.waker.wake();
                let distance_us = prx_next_normal_distance_us(state);
                prx_set_normal_request(state, distance_us);
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next = core::ptr::from_mut(&mut state.request);
            } else if long_session || state.counters.start < state.target_count {
                let distance_us = prx_next_normal_distance_us(state);
                prx_set_normal_request(state, distance_us);
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next = core::ptr::from_mut(&mut state.request);
            } else {
                state.done = true;
                state.waker.wake();
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            }
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_IDLE => PRX_STATE.with_inner(|state| {
            state.counters.session_idle += 1;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_BLOCKED | raw::MPSL_TIMESLOT_SIGNAL_CANCELLED => {
            let request = PRX_STATE.with_inner(|state| {
                if signal == raw::MPSL_TIMESLOT_SIGNAL_BLOCKED {
                    state.counters.blocked += 1;
                    match state.last_request_kind {
                        PRX_REQ_NORMAL => state.normal_blocked += 1,
                        PRX_REQ_EARLIEST => state.earliest_blocked += 1,
                        _ => {}
                    }
                } else {
                    state.counters.cancelled += 1;
                    match state.last_request_kind {
                        PRX_REQ_NORMAL => state.normal_cancelled += 1,
                        PRX_REQ_EARLIEST => state.earliest_cancelled += 1,
                        _ => {}
                    }
                }
                let priority = if state.retry_blocked_at_high_priority {
                    TIMESLOT_PRIORITY_HIGH
                } else {
                    TIMESLOT_PRIORITY_NORMAL
                };
                prx_set_earliest_request(
                    state,
                    priority,
                    raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US,
                );
                core::ptr::from_ref(&state.request)
            });
            let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
            if ret < 0 {
                PRX_STATE.with_inner(|state| {
                    state.counters.invalid_return += 1;
                    state.done = true;
                    state.waker.wake();
                });
            }
            core::ptr::null_mut()
        }

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_CLOSED => PRX_STATE.with_inner(|state| {
            state.counters.session_closed += 1;
            state.done = true;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_OVERSTAYED => PRX_STATE.with_inner(|state| {
            state.counters.overstayed += 1;
            state.slot_active = false;
            state.phase = PrxPhase::Idle;
            state.done = true;
            state.waker.wake();
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            &mut state.return_param as *mut _
        }),

        _ => PRX_STATE.with_inner(|state| {
            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            &mut state.return_param as *mut _
        }),
    }
}

/// Run PRX reception inside chained timeslots.
///
/// Each timeslot power-cycles the RADIO, inits ESB, and listens for
/// incoming packets. ACK is sent automatically for each received packet.
/// After `count` slots, returns cumulative results.
/// Duplicate detection state is preserved across slot boundaries.
pub async fn run_prx_slots(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    config: &EsbConfig,
    addresses: &EsbAddresses,
    slot_length_us: u32,
    in_slot_match_us: u32,
    count: u32,
    enabled_pipes: u8,
) -> Result<PrxSlotResult, Error> {
    let _busy = PRX_STATE.try_enter()?;
    if config.payload_length < 4 {
        return Err(Error::InvalidParam);
    }

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(prx_timeslot_callback), (&mut session_id) as *mut _)
    };
    mpsl_ok(ret)?;

    let _drop = OnDrop::new(|| {
        let _ = unsafe { raw::mpsl_timeslot_session_close(session_id) };
    });

    PRX_STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = count;
        state.slot_length_us = slot_length_us;
        state.in_slot_match_us = in_slot_match_us;
        state.request_timeout_us = 1_000_000;
        state.config = Some(config.clone());
        state.addresses = Some(addresses.clone());
        state.phase = PrxPhase::Idle;
        state.rx_count = 0;
        state.dup_count = 0;
        state.bad_crc_count = 0;
        state.last_pid = [0; NUM_PIPES];
        state.last_crc = [0; NUM_PIPES];
        state.last_valid = [false; NUM_PIPES];
        state.enabled_pipes = enabled_pipes;
        state.slot_active = false;
        state.ack_counter = [0; NUM_PIPES];
        state.rx_per_pipe = [0; NUM_PIPES];
        state.dup_per_pipe = [0; NUM_PIPES];
        state.bad_crc_per_pipe = [0; NUM_PIPES];
        state.ack_tx_per_pipe = [0; NUM_PIPES];
        state.report_every = 0;
        state.last_report_start = 0;
        state.report_ready = false;
        state.prx_schedule = PrxScheduleConfig::continuous();
        state.prx_windows_since_gap = 0;
        state.retry_blocked_at_high_priority = true;
        state.last_request_kind = PRX_REQ_EARLIEST;
        state.normal_blocked = 0;
        state.earliest_blocked = 0;
        state.normal_cancelled = 0;
        state.earliest_cancelled = 0;
        prx_set_earliest_request(state, TIMESLOT_PRIORITY_NORMAL, 1_000_000);
    });

    let request = PRX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    mpsl_ok(ret)?;

    poll_fn(|cx| {
        PRX_STATE.with_inner(|state| {
            state.waker.register(cx.waker());
            if state.done {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    })
    .await;

    _drop.defuse();
    unsafe {
        let ret = raw::mpsl_timeslot_session_close(session_id);
        mpsl_ok(ret)?;
    }

    Ok(PRX_STATE.with_inner(|state| PrxSlotResult {
        counters: state.counters,
        rx_count: state.rx_count,
        dup_count: state.dup_count,
        bad_crc_count: state.bad_crc_count,
        rx_per_pipe: state.rx_per_pipe,
        dup_per_pipe: state.dup_per_pipe,
        bad_crc_per_pipe: state.bad_crc_per_pipe,
        ack_tx_per_pipe: state.ack_tx_per_pipe,
        slot_length_us: state.slot_length_us,
        in_slot_match_us: state.in_slot_match_us,
        report_every: state.report_every,
        phase: state.phase.as_u8(),
        slot_active: state.slot_active,
        last_request_kind: state.last_request_kind,
        normal_blocked: state.normal_blocked,
        earliest_blocked: state.earliest_blocked,
        normal_cancelled: state.normal_cancelled,
        earliest_cancelled: state.earliest_cancelled,
    }))
}

/// Long-lived PRX timeslot session.
///
/// This keeps the MPSL session open and chains timeslots continuously. It is
/// intended for BLE coexistence tests where repeated session open/close can
/// contend with the SoftDevice Controller scheduler.
pub struct PrxSlotSession {
    _busy: BusyGuard,
    session_id: u8,
    last_counters: SignalCounters,
    last_rx_count: u32,
    last_dup_count: u32,
    last_bad_crc_count: u32,
    last_rx_per_pipe: [u32; NUM_PIPES],
    last_dup_per_pipe: [u32; NUM_PIPES],
    last_bad_crc_per_pipe: [u32; NUM_PIPES],
    last_ack_tx_per_pipe: [u32; NUM_PIPES],
    last_normal_blocked: u32,
    last_earliest_blocked: u32,
    last_normal_cancelled: u32,
    last_earliest_cancelled: u32,
}

impl PrxSlotSession {
    pub async fn next_report(&mut self) -> Result<PrxSlotResult, Error> {
        poll_fn(|cx| {
            PRX_STATE.with_inner(|state| {
                state.waker.register(cx.waker());
                if state.done {
                    Poll::Ready(())
                } else if state.report_ready {
                    state.report_ready = false;
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
        })
        .await;

        let result = PRX_STATE.with_inner(|state| {
            if state.done {
                return Err(Error::Mpsl);
            }

            let counters = state.counters.saturating_sub(self.last_counters);
            let rx_per_pipe = delta_per_pipe(&state.rx_per_pipe, &self.last_rx_per_pipe);
            let dup_per_pipe = delta_per_pipe(&state.dup_per_pipe, &self.last_dup_per_pipe);
            let bad_crc_per_pipe =
                delta_per_pipe(&state.bad_crc_per_pipe, &self.last_bad_crc_per_pipe);
            let ack_tx_per_pipe =
                delta_per_pipe(&state.ack_tx_per_pipe, &self.last_ack_tx_per_pipe);

            let result = PrxSlotResult {
                counters,
                rx_count: state.rx_count.saturating_sub(self.last_rx_count),
                dup_count: state.dup_count.saturating_sub(self.last_dup_count),
                bad_crc_count: state.bad_crc_count.saturating_sub(self.last_bad_crc_count),
                rx_per_pipe,
                dup_per_pipe,
                bad_crc_per_pipe,
                ack_tx_per_pipe,
                slot_length_us: state.slot_length_us,
                in_slot_match_us: state.in_slot_match_us,
                report_every: state.report_every,
                phase: state.phase.as_u8(),
                slot_active: state.slot_active,
                last_request_kind: state.last_request_kind,
                normal_blocked: state
                    .normal_blocked
                    .saturating_sub(self.last_normal_blocked),
                earliest_blocked: state
                    .earliest_blocked
                    .saturating_sub(self.last_earliest_blocked),
                normal_cancelled: state
                    .normal_cancelled
                    .saturating_sub(self.last_normal_cancelled),
                earliest_cancelled: state
                    .earliest_cancelled
                    .saturating_sub(self.last_earliest_cancelled),
            };

            self.last_counters = state.counters;
            self.last_rx_count = state.rx_count;
            self.last_dup_count = state.dup_count;
            self.last_bad_crc_count = state.bad_crc_count;
            self.last_rx_per_pipe = state.rx_per_pipe;
            self.last_dup_per_pipe = state.dup_per_pipe;
            self.last_bad_crc_per_pipe = state.bad_crc_per_pipe;
            self.last_ack_tx_per_pipe = state.ack_tx_per_pipe;
            self.last_normal_blocked = state.normal_blocked;
            self.last_earliest_blocked = state.earliest_blocked;
            self.last_normal_cancelled = state.normal_cancelled;
            self.last_earliest_cancelled = state.earliest_cancelled;

            Ok(result)
        })?;

        Ok(result)
    }
}

impl Drop for PrxSlotSession {
    fn drop(&mut self) {
        let _ = unsafe { raw::mpsl_timeslot_session_close(self.session_id) };
    }
}

pub fn open_prx_session(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    config: &EsbConfig,
    addresses: &EsbAddresses,
    slot_config: PrxSlotConfig,
) -> Result<PrxSlotSession, Error> {
    let busy = PRX_STATE.try_enter()?;
    if config.payload_length < 4 {
        drop(busy);
        return Err(Error::InvalidParam);
    }
    if let Err(e) = slot_config.validate() {
        drop(busy);
        return Err(e);
    }

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(prx_timeslot_callback), (&mut session_id) as *mut _)
    };
    if let Err(e) = mpsl_ok(ret) {
        drop(busy);
        return Err(e);
    }

    PRX_STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = 0;
        state.slot_length_us = slot_config.slot_length_us;
        state.in_slot_match_us = slot_config.in_slot_match_us;
        state.request_timeout_us = slot_config.request.timeout_us;
        state.config = Some(config.clone());
        state.addresses = Some(addresses.clone());
        state.phase = PrxPhase::Idle;
        state.rx_count = 0;
        state.dup_count = 0;
        state.bad_crc_count = 0;
        state.last_pid = [0; NUM_PIPES];
        state.last_crc = [0; NUM_PIPES];
        state.last_valid = [false; NUM_PIPES];
        state.enabled_pipes = slot_config.enabled_pipes;
        state.recovery_policy = slot_config.recovery;
        state.slot_active = false;
        state.ack_counter = [0; NUM_PIPES];
        state.rx_per_pipe = [0; NUM_PIPES];
        state.dup_per_pipe = [0; NUM_PIPES];
        state.bad_crc_per_pipe = [0; NUM_PIPES];
        state.ack_tx_per_pipe = [0; NUM_PIPES];
        state.report_every = slot_config.report_every;
        state.last_report_start = 0;
        state.report_ready = false;
        state.prx_schedule = slot_config.schedule;
        state.prx_windows_since_gap = 0;
        state.retry_blocked_at_high_priority = slot_config.request.retry_blocked_at_high_priority;
        state.last_request_kind = PRX_REQ_EARLIEST;
        state.normal_blocked = 0;
        state.earliest_blocked = 0;
        state.normal_cancelled = 0;
        state.earliest_cancelled = 0;
        prx_set_earliest_request(
            state,
            TIMESLOT_PRIORITY_NORMAL,
            slot_config.request.timeout_us,
        );
    });

    let request = PRX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    if let Err(e) = mpsl_ok(ret) {
        let _ = unsafe { raw::mpsl_timeslot_session_close(session_id) };
        drop(busy);
        return Err(e);
    }

    Ok(PrxSlotSession {
        _busy: busy,
        session_id,
        last_counters: SignalCounters::ZERO,
        last_rx_count: 0,
        last_dup_count: 0,
        last_bad_crc_count: 0,
        last_rx_per_pipe: [0; NUM_PIPES],
        last_dup_per_pipe: [0; NUM_PIPES],
        last_bad_crc_per_pipe: [0; NUM_PIPES],
        last_ack_tx_per_pipe: [0; NUM_PIPES],
        last_normal_blocked: 0,
        last_earliest_blocked: 0,
        last_normal_cancelled: 0,
        last_earliest_cancelled: 0,
    })
}

/// Per-round result for the PTX poll session.
pub struct PtxPollResult {
    /// Signal counters for this reporting period.
    pub counters: SignalCounters,
    /// Total TX packets in this period.
    pub tx_count: u32,
    /// Total ACK OK in this period.
    pub ack_ok_count: u32,
    /// Per-pipe TX count.
    pub tx_per_pipe: [u32; NUM_PIPES],
    /// Per-pipe ACK OK count.
    pub ack_per_pipe: [u32; NUM_PIPES],
    /// ACK wait windows that reached the diagnostic timeout.
    pub ack_timeout_count: u32,
    /// ACK packets received with a bad CRC.
    pub ack_crc_fail_count: u32,
    /// Per-pipe ACK wait windows that reached the diagnostic timeout.
    pub ack_timeout_per_pipe: [u32; NUM_PIPES],
    /// Per-pipe ACK packets received with a bad CRC.
    pub ack_crc_fail_per_pipe: [u32; NUM_PIPES],
    /// ACK payloads that carried a valid passive schedule hint.
    pub schedule_hint_count: u32,
    /// ACK payloads whose schedule hint was missing or invalid.
    pub schedule_hint_bad_count: u32,
    /// Last valid PRX window id observed in a schedule hint.
    pub last_schedule_window_id: u32,
    /// Passive schedule relationship diagnostics for this report.
    pub schedule: ScheduleTrackerSnapshot,
    /// PTX timeslots intentionally skipped by the schedule gate.
    pub schedule_skip_count: u32,
    /// Whether phase-locked scheduling is currently active.
    pub schedule_lock_active: bool,
    /// New phase-lock acquisitions in this period.
    pub schedule_lock_count: u32,
    /// Phase-lock fallbacks to scanning in this period.
    pub schedule_reacquire_count: u32,
    /// Consecutive misses while phase-locked.
    pub schedule_lock_miss_streak: u8,
    /// Current phase-locked request distance.
    pub schedule_period_us: u32,
}

/// Long-lived PTX poll session that round-robins across pipes.
///
/// Each timeslot polls exactly one pipe (1 TX + wait ACK). The pipe
/// auto-advances through `pipe_mask` in round-robin order. After
/// `report_every` slots, `next_report()` returns incremental stats.
pub struct PtxPollSession {
    _busy: BusyGuard,
    session_id: u8,
    last_counters: SignalCounters,
    last_tx_count: u32,
    last_ack_ok_count: u32,
    last_tx_per_pipe: [u32; NUM_PIPES],
    last_ack_per_pipe: [u32; NUM_PIPES],
    last_ack_timeout_count: u32,
    last_ack_crc_fail_count: u32,
    last_schedule_hint_count: u32,
    last_schedule_hint_bad_count: u32,
    last_schedule: ScheduleTrackerSnapshot,
    last_schedule_skip_count: u32,
    last_schedule_lock_count: u32,
    last_schedule_reacquire_count: u32,
    last_ack_timeout_per_pipe: [u32; NUM_PIPES],
    last_ack_crc_fail_per_pipe: [u32; NUM_PIPES],
}

impl PtxPollSession {
    pub async fn next_report(&mut self) -> Result<PtxPollResult, Error> {
        poll_fn(|cx| {
            PTX_STATE.with_inner(|state| {
                state.waker.register(cx.waker());
                if state.done {
                    Poll::Ready(())
                } else if state.poll_report_ready {
                    state.poll_report_ready = false;
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
        })
        .await;

        let result = PTX_STATE.with_inner(|state| {
            if state.done {
                return Err(Error::Mpsl);
            }

            let counters = state.counters.saturating_sub(self.last_counters);
            let tx_per_pipe = delta_per_pipe(&state.tx_per_pipe, &self.last_tx_per_pipe);
            let ack_per_pipe = delta_per_pipe(&state.ack_ok_per_pipe, &self.last_ack_per_pipe);
            let ack_timeout_per_pipe =
                delta_per_pipe(&state.ack_timeout_per_pipe, &self.last_ack_timeout_per_pipe);
            let ack_crc_fail_per_pipe = delta_per_pipe(
                &state.ack_crc_fail_per_pipe,
                &self.last_ack_crc_fail_per_pipe,
            );
            let schedule = state
                .schedule_tracker
                .snapshot()
                .saturating_sub(self.last_schedule);

            let result = PtxPollResult {
                counters,
                tx_count: state.tx_count.saturating_sub(self.last_tx_count),
                ack_ok_count: state.ack_ok_count.saturating_sub(self.last_ack_ok_count),
                tx_per_pipe,
                ack_per_pipe,
                ack_timeout_count: state
                    .ack_timeout_count
                    .saturating_sub(self.last_ack_timeout_count),
                ack_crc_fail_count: state
                    .ack_crc_fail_count
                    .saturating_sub(self.last_ack_crc_fail_count),
                ack_timeout_per_pipe,
                ack_crc_fail_per_pipe,
                schedule_hint_count: state
                    .schedule_hint_count
                    .saturating_sub(self.last_schedule_hint_count),
                schedule_hint_bad_count: state
                    .schedule_hint_bad_count
                    .saturating_sub(self.last_schedule_hint_bad_count),
                last_schedule_window_id: state.last_schedule_window_id,
                schedule,
                schedule_skip_count: state
                    .schedule_skip_count
                    .saturating_sub(self.last_schedule_skip_count),
                schedule_lock_active: state.schedule_lock_active,
                schedule_lock_count: state
                    .schedule_lock_count
                    .saturating_sub(self.last_schedule_lock_count),
                schedule_reacquire_count: state
                    .schedule_reacquire_count
                    .saturating_sub(self.last_schedule_reacquire_count),
                schedule_lock_miss_streak: state.schedule_lock_miss_streak,
                schedule_period_us: state.schedule_period_us,
            };

            self.last_counters = state.counters;
            self.last_tx_count = state.tx_count;
            self.last_ack_ok_count = state.ack_ok_count;
            self.last_tx_per_pipe = state.tx_per_pipe;
            self.last_ack_per_pipe = state.ack_ok_per_pipe;
            self.last_ack_timeout_count = state.ack_timeout_count;
            self.last_ack_crc_fail_count = state.ack_crc_fail_count;
            self.last_schedule_hint_count = state.schedule_hint_count;
            self.last_schedule_hint_bad_count = state.schedule_hint_bad_count;
            self.last_schedule = state.schedule_tracker.snapshot();
            self.last_schedule_skip_count = state.schedule_skip_count;
            self.last_schedule_lock_count = state.schedule_lock_count;
            self.last_schedule_reacquire_count = state.schedule_reacquire_count;
            self.last_ack_timeout_per_pipe = state.ack_timeout_per_pipe;
            self.last_ack_crc_fail_per_pipe = state.ack_crc_fail_per_pipe;

            Ok(result)
        })?;

        Ok(result)
    }
}

impl Drop for PtxPollSession {
    fn drop(&mut self) {
        let _ = unsafe { raw::mpsl_timeslot_session_close(self.session_id) };
    }
}

/// Open a long-lived PTX poll session that round-robins across the given pipes.
///
/// `poll_config` controls the slot length, pipe mask, report cadence, ACK
/// timeout, and optional in-slot retries.
pub fn open_ptx_poll_session(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    config: &EsbConfig,
    addresses: &EsbAddresses,
    poll_config: PtxPollConfig,
) -> Result<PtxPollSession, Error> {
    let busy = PTX_STATE.try_enter()?;
    if config.payload_length < 4 {
        drop(busy);
        return Err(Error::InvalidParam);
    }
    if let Err(e) = poll_config.validate() {
        drop(busy);
        return Err(e);
    }

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(ptx_timeslot_callback), (&mut session_id) as *mut _)
    };
    if let Err(e) = mpsl_ok(ret) {
        drop(busy);
        return Err(e);
    }

    let first_pipe = poll_config.pipe_mask.trailing_zeros() as u8;

    PTX_STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = 0;
        state.slot_length_us = poll_config.slot_length_us;
        state.in_slot_match_us = poll_config.in_slot_match_us;
        state.request_timeout_us = poll_config.request.timeout_us;
        state.config = Some(config.clone());
        state.addresses = Some(addresses.clone());
        state.phase = PtxPhase::Idle;
        state.tx_count = 0;
        state.ack_ok_count = 0;
        state.tx_pipe = first_pipe;
        state.payload_byte = 0;
        state.packets_per_slot = 1;
        state.packets_sent_this_slot = 0;
        state.ack_payload_count = 0;
        state.ack_inversions = 0;
        state.last_ack_counter = 0;
        state.schedule_hint_count = 0;
        state.schedule_hint_bad_count = 0;
        state.last_schedule_window_id = 0;
        state.schedule_tracker.reset();
        state.schedule_gate = poll_config.schedule_gate;
        state.schedule_skip_slots_remaining = 0;
        state.schedule_skip_count = 0;
        state.schedule_lock_active = false;
        state.schedule_lock_miss_streak = 0;
        state.schedule_lock_count = 0;
        state.schedule_reacquire_count = 0;
        state.schedule_period_us = 0;
        state.schedule_next_distance_us = 0;
        state.pid = 0;
        state.poll_pipes = poll_config.pipe_mask.count_ones() as u8;
        state.poll_pipe_mask = poll_config.pipe_mask;
        state.poll_report_every = poll_config.report_every;
        state.poll_slots_since_report = 0;
        state.poll_report_ready = false;
        state.ack_ok_per_pipe = [0; NUM_PIPES];
        state.tx_per_pipe = [0; NUM_PIPES];
        state.ack_timeout_per_pipe = [0; NUM_PIPES];
        state.ack_crc_fail_per_pipe = [0; NUM_PIPES];
        state.retry_count = 0;
        state.max_retries = poll_config.max_retries;
        state.acked_this_slot = false;
        state.ack_timeout_us = poll_config.ack_timeout_us;
        ptx_set_earliest_request(
            state,
            TIMESLOT_PRIORITY_NORMAL,
            poll_config.request.timeout_us,
        );
    });

    let request = PTX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    if let Err(e) = mpsl_ok(ret) {
        let _ = unsafe { raw::mpsl_timeslot_session_close(session_id) };
        drop(busy);
        return Err(e);
    }

    Ok(PtxPollSession {
        _busy: busy,
        session_id,
        last_counters: SignalCounters::ZERO,
        last_tx_count: 0,
        last_ack_ok_count: 0,
        last_tx_per_pipe: [0; NUM_PIPES],
        last_ack_per_pipe: [0; NUM_PIPES],
        last_ack_timeout_count: 0,
        last_ack_crc_fail_count: 0,
        last_schedule_hint_count: 0,
        last_schedule_hint_bad_count: 0,
        last_schedule: ScheduleTrackerSnapshot::ZERO,
        last_schedule_skip_count: 0,
        last_schedule_lock_count: 0,
        last_schedule_reacquire_count: 0,
        last_ack_timeout_per_pipe: [0; NUM_PIPES],
        last_ack_crc_fail_per_pipe: [0; NUM_PIPES],
    })
}

// ---- Event-driven PTX ----

/// Result of a single event-driven PTX send attempt.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PtxSendResult {
    /// Whether the packet was ACKed by the PRX.
    pub ack_ok: bool,
    /// Signal counters observed during this send attempt.
    pub counters: SignalCounters,
}

/// A long-lived event-driven PTX session.
///
/// The session keeps the MPSL timeslot session open but does **not**
/// continuously chain slots. Instead, each `send()` call requests one
/// timeslot, transmits the pending payload, and returns the ACK result.
///
/// If ACK is not received, the caller should retry by calling `send()`
/// again after a short delay to align with the next PRX receive window.
pub struct PtxEventSession {
    _busy: BusyGuard,
    session_id: u8,
    pid: u8,
    tx_count: u32,
}

/// Open an event-driven PTX session.
///
/// Unlike `open_ptx_poll_session`, this does **not** start sending
/// immediately. The caller calls `send()` when a key event (or other
/// application event) triggers a transmission.
pub fn open_event_session(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    config: &EsbConfig,
    addresses: &EsbAddresses,
    event_config: PtxEventConfig,
) -> Result<PtxEventSession, Error> {
    let busy = PTX_STATE.try_enter()?;
    if config.payload_length < 4 {
        drop(busy);
        return Err(Error::InvalidParam);
    }
    if let Err(e) = event_config.validate() {
        drop(busy);
        return Err(e);
    }

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(ptx_timeslot_callback), (&mut session_id) as *mut _)
    };
    if let Err(e) = mpsl_ok(ret) {
        drop(busy);
        return Err(e);
    }

    PTX_STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = 0;
        state.in_slot_match_us = event_config.in_slot_match_us;
        state.config = Some(config.clone());
        state.addresses = Some(addresses.clone());
        state.phase = PtxPhase::Idle;
        state.tx_count = 0;
        state.ack_ok_count = 0;
        state.tx_pipe = event_config.pipe;
        state.payload_byte = 0;
        state.packets_per_slot = 1;
        state.packets_sent_this_slot = 0;
        state.ack_payload_count = 0;
        state.ack_inversions = 0;
        state.last_ack_counter = 0;
        state.pid = 0;
        state.poll_pipes = 0;
        state.poll_pipe_mask = 0;
        state.poll_report_every = 0;
        state.poll_slots_since_report = 0;
        state.poll_report_ready = false;
        state.ack_ok_per_pipe = [0; NUM_PIPES];
        state.tx_per_pipe = [0; NUM_PIPES];
        state.ack_timeout_per_pipe = [0; NUM_PIPES];
        state.ack_crc_fail_per_pipe = [0; NUM_PIPES];
        state.retry_count = 0;
        state.max_retries = event_config.max_retries;
        state.acked_this_slot = false;
        state.ack_timeout_us = event_config.ack_timeout_us;
        state.event_mode = true;
        state.event_pending = false;
        state.event_result_ready = false;
        state.event_ack_ok = false;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = event_config.slot_length_us;
        state.request.params.earliest.timeout_us = event_config.request.timeout_us;
    });

    Ok(PtxEventSession {
        _busy: busy,
        session_id,
        pid: 0,
        tx_count: 0,
    })
}

impl PtxEventSession {
    /// Send a key report payload and wait for ACK.
    ///
    /// `payload` is written into the ESB TX buffer starting at the
    /// payload offset. The first byte after the ESB header will be
    /// the payload length seen by the PRX.
    ///
    /// If ACK is not received within the timeslot (including in-slot
    /// retries), returns `ack_ok: false`. The caller should retry.
    pub async fn send(&mut self, payload: &[u8]) -> Result<PtxSendResult, Error> {
        let max_app_payload = PTX_STATE.with_inner(|state| {
            state
                .config
                .as_ref()
                .map(|c| c.payload_length as usize)
                .unwrap_or(0)
                .saturating_sub(4)
        });
        if payload.len() > max_app_payload {
            return Err(Error::InvalidParam);
        }

        let staged = PTX_STATE.with_inner(|state| {
            let tx_buf = unsafe { &mut *PTX_BUFS.tx.get() };
            if !write_counter_payload_packet(tx_buf, self.pid, self.tx_count, payload) {
                return false;
            }

            state.event_pending = true;
            state.event_result_ready = false;
            state.event_ack_ok = false;
            state.done = false;
            state.counters = SignalCounters::ZERO;
            true
        });
        if !staged {
            return Err(Error::InvalidParam);
        }

        let request = PTX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
        let ret = unsafe { raw::mpsl_timeslot_request(self.session_id, request) };
        if let Err(e) = mpsl_ok(ret) {
            PTX_STATE.with_inner(|state| {
                state.event_pending = false;
                state.event_result_ready = false;
                state.event_ack_ok = false;
            });
            return Err(e);
        }

        poll_fn(|cx| {
            PTX_STATE.with_inner(|state| {
                state.waker.register(cx.waker());
                if state.event_result_ready || state.done {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
        })
        .await;

        let result = PTX_STATE.with_inner(|state| {
            if state.done && !state.event_result_ready {
                return Err(Error::Mpsl);
            }

            let ack_ok = state.event_ack_ok;
            if ack_ok {
                self.pid = advance_pid(self.pid);
            }
            self.tx_count += 1;

            let counters = state.counters;

            state.event_result_ready = false;
            state.event_pending = false;

            Ok(PtxSendResult { ack_ok, counters })
        })?;

        Ok(result)
    }
}

impl Drop for PtxEventSession {
    fn drop(&mut self) {
        PTX_STATE.with_inner(|state| {
            state.event_mode = false;
            state.event_pending = false;
        });
        let _ = unsafe { raw::mpsl_timeslot_session_close(self.session_id) };
    }
}
