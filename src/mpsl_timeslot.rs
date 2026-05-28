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

const TIMESLOT_TIMER_INTERRUPT: Interrupt = Interrupt::TIMER0;
const RADIO_DISABLE_SPIN_LIMIT: u32 = 100_000;

fn mpsl_ok(ret: i32) -> Result<(), Error> {
    RetVal::from(ret)
        .to_result()
        .map(|_| ())
        .map_err(|_| Error::Mpsl)
}

fn quiesce_radio_before_timeslot_end() {
    let r = pac::RADIO;
    r.shorts().modify(|w| {
        w.set_ready_start(false);
        w.set_end_disable(false);
        w.set_disabled_rxen(false);
        w.set_disabled_txen(false);
    });
    r.intenclr().write(|w| w.set_disabled(true));
    r.events_disabled().write_value(0);
    r.tasks_disable().write_value(1);

    let mut spins = 0;
    while r.events_disabled().read() == 0 && spins < RADIO_DISABLE_SPIN_LIMIT {
        spins += 1;
    }

    r.events_address().write_value(0);
    r.events_payload().write_value(0);
    r.events_end().write_value(0);
    r.events_ready().write_value(0);
    r.events_disabled().write_value(0);

    // Reset residual RADIO state programmed by the ESB slot before MPSL
    // hands the peripheral back to SDC.
    r.power().write(|w| w.set_power(false));
    r.power().write(|w| w.set_power(true));
    cortex_m::peripheral::NVIC::unpend(pac::Interrupt::RADIO);
    compiler_fence(Ordering::Acquire);
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
}

impl SignalCounters {
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
        }
    }

    const ZERO: Self = Self {
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
    };
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
    in_slot_match_us: u32,
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
    poll_pipes: u8,
    poll_pipe_mask: u8,
    poll_report_every: u32,
    poll_slots_since_report: u32,
    poll_report_ready: bool,
    ack_ok_per_pipe: [u32; NUM_PIPES],
    tx_per_pipe: [u32; NUM_PIPES],
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

fn write_counter_packet(buf: &mut [u8; 256], pid: u8, counter: u32) {
    let header = unsafe { &mut *(buf.as_mut_ptr().cast::<EsbHeader>()) };
    header.length = 4;
    header.pid_no_ack = 0;
    header.set_pid(pid);
    header.set_no_ack(false);

    let p = EsbHeader::PAYLOAD_OFFSET;
    buf[p] = counter as u8;
    buf[p + 1] = (counter >> 8) as u8;
    buf[p + 2] = (counter >> 16) as u8;
    buf[p + 3] = (counter >> 24) as u8;
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
                in_slot_match_us: 5500,
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
                poll_pipes: 0,
                poll_pipe_mask: 0,
                poll_report_every: 0,
                poll_slots_since_report: 0,
                poll_report_ready: false,
                ack_ok_per_pipe: [0; NUM_PIPES],
                tx_per_pipe: [0; NUM_PIPES],
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
                let mask = state.poll_pipe_mask;
                let mut next = state.tx_pipe;
                for _ in 0..NUM_PIPES {
                    next = (next + 1) % NUM_PIPES as u8;
                    if mask & (1 << next) != 0 {
                        break;
                    }
                }
                state.tx_pipe = next;
            }

            // Power cycle RADIO.
            let r = pac::RADIO;
            r.power().write(|w| w.set_power(false));
            r.power().write(|w| w.set_power(true));

            // Full ESB register init.
            let mut radio = crate::radio::EsbRadio::new(pac::RADIO);
            radio.init(config, addresses);
            radio.restore_pid_state([state.pid; 8]);

            // Prepare TX buffer — payload is a u32 packet counter (LE).
            let tx_buf = unsafe { &mut *PTX_BUFS.tx.get() };
            let dma_off = crate::header::EsbHeader::DMA_OFFSET;
            let counter = state.tx_count;
            write_counter_packet(tx_buf, state.pid, counter);

            // Arm TIMER0 for slot end.
            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.cc(0).write_value(state.in_slot_match_us);
            t.intenset().write(|w| w.set_compare(0, true));

            // Trigger TX with ACK.
            let dma_ptr = unsafe { tx_buf.as_mut_ptr().add(dma_off) };
            radio.transmit(state.tx_pipe, dma_ptr, true);
            state.tx_count += 1;
            state.packets_sent_this_slot = 1;
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

                    state.phase = PtxPhase::WaitAck;
                }
                PtxPhase::WaitAck if disabled => {
                    r.events_disabled().write_value(0);

                    let crc_ok =
                        r.crcstatus().read().crcstatus() == pac::radio::vals::Crcstatus::CRCOK;
                    compiler_fence(Ordering::Acquire);

                    if crc_ok {
                        state.ack_ok_count += 1;
                        if state.poll_pipes > 0 {
                            let pipe = state.tx_pipe as usize;
                            if pipe < NUM_PIPES {
                                state.ack_ok_per_pipe[pipe] += 1;
                            }
                        }

                        let rx_buf = unsafe { &*PTX_BUFS.rx.get() };
                        let length = rx_buf[EsbHeader::DMA_OFFSET] as usize;
                        if length >= 4 {
                            let p = EsbHeader::PAYLOAD_OFFSET;
                            let counter = u32::from_le_bytes([
                                rx_buf[p],
                                rx_buf[p + 1],
                                rx_buf[p + 2],
                                rx_buf[p + 3],
                            ]);
                            state.ack_payload_count += 1;
                            if counter < state.last_ack_counter {
                                state.ack_inversions += 1;
                            }
                            state.last_ack_counter = counter;
                        }
                    }

                    state.pid = (state.pid + 1) & 0x03;
                    state.payload_byte = state.payload_byte.wrapping_add(1);
                    state.packets_sent_this_slot += 1;

                    if state.packets_sent_this_slot < state.packets_per_slot {
                        // Send next packet within this slot.
                        state.phase = PtxPhase::Tx;
                        let counter = state.tx_count;
                        state.tx_count += 1;

                        let tx_buf = unsafe { &mut *PTX_BUFS.tx.get() };
                        let dma_off = crate::header::EsbHeader::DMA_OFFSET;
                        write_counter_packet(tx_buf, state.pid, counter);

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
                    } else {
                        // All packets for this slot done — stop radio.
                        r.shorts().modify(|w| {
                            w.set_ready_start(false);
                            w.set_end_disable(false);
                        });
                        r.intenclr().write(|w| w.set_disabled(true));
                        r.tasks_disable().write_value(1);
                        while r.events_disabled().read() == 0 {}
                        r.events_disabled().write_value(0);
                        compiler_fence(Ordering::Acquire);

                        state.phase = PtxPhase::Done;
                    }
                }
                _ => {}
            }

            state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_NONE as u8;
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_TIMER0 => PTX_STATE.with_inner(|state| {
            state.counters.timer0 += 1;

            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.intenclr().write(|w| w.set_compare(0, true));

            // Wind down any in-flight radio op so the slot ends in a clean
            // state. Otherwise mid-cycle RX (waiting for ACK) would be
            // clobbered by the next slot's power_cycle.
            if state.phase != PtxPhase::Done && state.phase != PtxPhase::Idle {
                let r = pac::RADIO;
                r.shorts().modify(|w| {
                    w.set_ready_start(false);
                    w.set_end_disable(false);
                    w.set_disabled_rxen(false);
                    w.set_disabled_txen(false);
                });
                r.intenclr().write(|w| w.set_disabled(true));
                r.tasks_disable().write_value(1);
                while r.events_disabled().read() == 0 {}
                r.events_disabled().write_value(0);
                compiler_fence(Ordering::Acquire);
                state.phase = PtxPhase::Done;
            }

            let chain = state.target_count > 0 && state.counters.start < state.target_count;
            let poll_active = state.poll_pipes > 0;

            if poll_active {
                if state.phase != PtxPhase::Done && state.phase != PtxPhase::Idle {
                    let r = pac::RADIO;
                    r.shorts().modify(|w| {
                        w.set_ready_start(false);
                        w.set_end_disable(false);
                        w.set_disabled_rxen(false);
                        w.set_disabled_txen(false);
                    });
                    r.intenclr().write(|w| w.set_disabled(true));
                    r.events_disabled().write_value(0);
                    r.tasks_disable().write_value(1);
                    let mut spins = 0u32;
                    while r.events_disabled().read() == 0 && spins < 1000 {
                        spins += 1;
                    }
                    r.events_disabled().write_value(0);
                    compiler_fence(Ordering::Acquire);
                    state.phase = PtxPhase::Idle;
                }

                state.poll_slots_since_report += 1;
                if state.poll_slots_since_report >= state.poll_report_every {
                    state.poll_slots_since_report = 0;
                    state.poll_report_ready = true;
                    state.waker.wake();
                }

                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next = core::ptr::from_mut(&mut state.request);
            } else if chain {
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next = core::ptr::from_mut(&mut state.request);
            } else {
                state.done = true;
                state.waker.wake();
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            }
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_IDLE => PTX_STATE.with_inner(|state| {
            state.counters.session_idle += 1;
            if state.poll_pipes > 0 {
                state.poll_report_ready = true;
            }
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_BLOCKED | raw::MPSL_TIMESLOT_SIGNAL_CANCELLED => {
            let request = PTX_STATE.with_inner(|state| {
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
                    state.request.params.earliest.priority =
                        raw::MPSL_TIMESLOT_PRIORITY_HIGH as u8;
                    state.request.params.earliest.timeout_us =
                        raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US;
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
        state.in_slot_match_us = in_slot_match_us;
        state.config = Some(config.clone());
        state.addresses = Some(addresses.clone());
        state.phase = PtxPhase::Idle;
        state.tx_count = 0;
        state.ack_ok_count = 0;
        state.ack_payload_count = 0;
        state.ack_inversions = 0;
        state.last_ack_counter = 0;
        state.tx_pipe = tx_pipe;
        state.payload_byte = 0;
        state.packets_per_slot = packets_per_slot;
        state.packets_sent_this_slot = 0;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
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

const NUM_PIPES: usize = 8;

/// Result of a PRX timeslot session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PrxSlotResult {
    pub counters: SignalCounters,
    pub rx_count: u32,
    pub dup_count: u32,
    pub bad_crc_count: u32,
    pub rx_per_pipe: [u32; NUM_PIPES],
}

/// Phase within a single PRX timeslot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrxPhase {
    Idle,
    Receiving,
    TxAck,
    TxRepeatedAck,
}

struct PrxInnerState {
    counters: SignalCounters,
    done: bool,
    waker: WakerRegistration,
    request: raw::mpsl_timeslot_request_t,
    return_param: raw::mpsl_timeslot_signal_return_param_t,
    in_slot_match_us: u32,
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
    slot_active: bool,
    ack_counter: [u32; NUM_PIPES],
    rx_per_pipe: [u32; NUM_PIPES],
    report_every: u32,
    last_report_start: u32,
    report_ready: bool,
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

fn write_ack_counter_packet(buf: &mut [u8; 256], counter: u32) {
    write_counter_packet(buf, 0, counter);
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
                in_slot_match_us: 5500,
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
                slot_active: false,
                ack_counter: [0; NUM_PIPES],
                rx_per_pipe: [0; NUM_PIPES],
                report_every: 0,
                last_report_start: 0,
                report_ready: false,
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
                                write_ack_counter_packet(ack_buf, counter);
                                let dma_ptr =
                                    unsafe { ack_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                                let mut radio = EsbRadio::new(pac::RADIO);
                                radio.transmit_ack_manual(pipe as u8, dma_ptr);
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
                                write_ack_counter_packet(ack_buf, counter);
                                let dma_ptr =
                                    unsafe { ack_buf.as_mut_ptr().add(EsbHeader::DMA_OFFSET) };
                                let mut radio = EsbRadio::new(pac::RADIO);
                                radio.transmit_ack_manual(pipe as u8, dma_ptr);
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

            quiesce_radio_before_timeslot_end();
            state.phase = PrxPhase::Idle;

            let long_session = state.report_every > 0;
            let batch_complete = long_session
                && state.counters.start.saturating_sub(state.last_report_start)
                    >= state.report_every;

            if batch_complete {
                state.last_report_start = state.counters.start;
                state.report_ready = true;
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            } else if long_session || state.counters.start < state.target_count {
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
        state.in_slot_match_us = in_slot_match_us;
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
        state.report_every = 0;
        state.last_report_start = 0;
        state.report_ready = false;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
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
    needs_request: bool,
    last_counters: SignalCounters,
    last_rx_count: u32,
    last_dup_count: u32,
    last_bad_crc_count: u32,
    last_rx_per_pipe: [u32; NUM_PIPES],
}

impl PrxSlotSession {
    pub async fn next_report(&mut self) -> Result<PrxSlotResult, Error> {
        if self.needs_request {
            PRX_STATE.with_inner(|state| {
                state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
                state.request.params.earliest.timeout_us = 1_000_000;
            });
            let request = PRX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
            let ret = unsafe { raw::mpsl_timeslot_request(self.session_id, request) };
            mpsl_ok(ret)?;
            self.needs_request = false;
        }

        poll_fn(|cx| {
            PRX_STATE.with_inner(|state| {
                state.waker.register(cx.waker());
                if state.done {
                    Poll::Ready(())
                } else if state.report_ready
                    && state.counters.session_idle > self.last_counters.session_idle
                {
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
            let mut rx_per_pipe = [0; NUM_PIPES];
            for (idx, rx) in rx_per_pipe.iter_mut().enumerate() {
                *rx = state.rx_per_pipe[idx].saturating_sub(self.last_rx_per_pipe[idx]);
            }

            let result = PrxSlotResult {
                counters,
                rx_count: state.rx_count.saturating_sub(self.last_rx_count),
                dup_count: state.dup_count.saturating_sub(self.last_dup_count),
                bad_crc_count: state.bad_crc_count.saturating_sub(self.last_bad_crc_count),
                rx_per_pipe,
            };

            self.last_counters = state.counters;
            self.last_rx_count = state.rx_count;
            self.last_dup_count = state.dup_count;
            self.last_bad_crc_count = state.bad_crc_count;
            self.last_rx_per_pipe = state.rx_per_pipe;

            Ok(result)
        })?;

        self.needs_request = true;
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
    slot_length_us: u32,
    in_slot_match_us: u32,
    report_every: u32,
    enabled_pipes: u8,
) -> Result<PrxSlotSession, Error> {
    let busy = PRX_STATE.try_enter()?;
    if config.payload_length < 4 || report_every == 0 {
        return Err(Error::InvalidParam);
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
        state.in_slot_match_us = in_slot_match_us;
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
        state.report_every = report_every;
        state.last_report_start = 0;
        state.report_ready = false;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
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
        needs_request: false,
        last_counters: SignalCounters::ZERO,
        last_rx_count: 0,
        last_dup_count: 0,
        last_bad_crc_count: 0,
        last_rx_per_pipe: [0; NUM_PIPES],
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
            let mut tx_per_pipe = [0u32; NUM_PIPES];
            let mut ack_per_pipe = [0u32; NUM_PIPES];
            for i in 0..NUM_PIPES {
                tx_per_pipe[i] = state.tx_per_pipe[i].saturating_sub(self.last_tx_per_pipe[i]);
                ack_per_pipe[i] = state.ack_ok_per_pipe[i].saturating_sub(self.last_ack_per_pipe[i]);
            }

            let result = PtxPollResult {
                counters,
                tx_count: state.tx_count.saturating_sub(self.last_tx_count),
                ack_ok_count: state
                    .ack_ok_count
                    .saturating_sub(self.last_ack_ok_count),
                tx_per_pipe,
                ack_per_pipe,
            };

            self.last_counters = state.counters;
            self.last_tx_count = state.tx_count;
            self.last_ack_ok_count = state.ack_ok_count;
            self.last_tx_per_pipe = state.tx_per_pipe;
            self.last_ack_per_pipe = state.ack_ok_per_pipe;

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
/// - `slot_length_us`: timeslot duration per poll (e.g. 1500 for ~1ms poll).
/// - `in_slot_match_us`: TIMER0 compare value within slot (should be < slot_length_us).
/// - `pipe_mask`: bitmask of pipes to poll (e.g. `0b0111_1110` for pipes 1-7).
/// - `report_every`: number of slots per report (e.g. 7 for one full round).
pub fn open_ptx_poll_session(
    _mpsl: &MultiprotocolServiceLayer<'_>,
    config: &EsbConfig,
    addresses: &EsbAddresses,
    slot_length_us: u32,
    in_slot_match_us: u32,
    pipe_mask: u8,
    report_every: u32,
) -> Result<PtxPollSession, Error> {
    let busy = PTX_STATE.try_enter()?;
    if config.payload_length < 4 || pipe_mask == 0 || report_every == 0 {
        drop(busy);
        return Err(Error::InvalidParam);
    }

    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(ptx_timeslot_callback), (&mut session_id) as *mut _)
    };
    if let Err(e) = mpsl_ok(ret) {
        drop(busy);
        return Err(e);
    }

    let first_pipe = pipe_mask.trailing_zeros() as u8;

    PTX_STATE.with_inner(|state| {
        state.counters = SignalCounters::ZERO;
        state.done = false;
        state.target_count = 0;
        state.in_slot_match_us = in_slot_match_us;
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
        state.pid = 0;
        state.poll_pipes = pipe_mask.count_ones() as u8;
        state.poll_pipe_mask = pipe_mask;
        state.poll_report_every = report_every;
        state.poll_slots_since_report = 0;
        state.poll_report_ready = false;
        state.ack_ok_per_pipe = [0; NUM_PIPES];
        state.tx_per_pipe = [0; NUM_PIPES];
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
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
    })
}
