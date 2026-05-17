//! MPSL timeslot adapter for ESB.
//!
//! Manages timeslot sessions via nrf-mpsl, providing async APIs for
//! requesting individual timeslots, chained timeslot sequences, and
//! PTX transmissions within timeslots.

use core::cell::RefCell;
use core::future::poll_fn;
use core::sync::atomic::{compiler_fence, Ordering};
use core::task::Poll;

use cortex_m::peripheral::NVIC;
use embassy_nrf::interrupt::Interrupt;
use embassy_nrf::pac;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::waitqueue::WakerRegistration;

use nrf_mpsl::{raw, MultiprotocolServiceLayer, RetVal};

const TIMESLOT_TIMER_INTERRUPT: Interrupt = Interrupt::TIMER0;

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
    inner: Mutex<Timer0RawMutex, RefCell<InnerState>>,
}

static STATE: State = State::new();

impl State {
    const fn new() -> Self {
        Self {
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
                        request: raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1__bindgen_ty_1 {
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
                state.return_param.callback_action =
                    raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next =
                    core::ptr::from_mut(&mut state.request);
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
                state.request.params.earliest.priority =
                    raw::MPSL_TIMESLOT_PRIORITY_HIGH as u8;
                state.request.params.earliest.timeout_us =
                    raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US;
                core::ptr::from_ref(&state.request)
            });
            let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
            assert!(ret == 0);
            core::ptr::null_mut()
        }

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_CLOSED => STATE.with_inner(|state| {
            state.counters.session_closed += 1;
            state.done = true;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_OVERSTAYED => {
            panic!("timeslot overstayed");
        }

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
) -> SignalCounters {
    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(timeslot_callback), (&mut session_id) as *mut _)
    };
    RetVal::from(ret).to_result().unwrap();

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
    RetVal::from(ret).to_result().unwrap();

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
        RetVal::from(ret).to_result().unwrap();
    }

    STATE.with_inner(|state| state.counters)
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
) -> SignalCounters {
    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(Some(timeslot_callback), (&mut session_id) as *mut _)
    };
    RetVal::from(ret).to_result().unwrap();

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
    RetVal::from(ret).to_result().unwrap();

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
        RetVal::from(ret).to_result().unwrap();
    }

    STATE.with_inner(|state| state.counters)
}

// ---- PTX-in-timeslot ----

use core::cell::UnsafeCell;

/// Result of a PTX timeslot session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PtxSlotResult {
    pub counters: SignalCounters,
    pub tx_count: u32,
    pub ack_ok_count: u32,
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
}

unsafe impl Send for PtxInnerState {}
unsafe impl Sync for PtxInnerState {}

struct PtxState {
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

impl PtxState {
    const fn new() -> Self {
        Self {
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
                        request: raw::mpsl_timeslot_signal_return_param_t__bindgen_ty_1__bindgen_ty_1 {
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
            })),
        }
    }

    fn with_inner<F: FnOnce(&mut PtxInnerState) -> R, R>(&self, f: F) -> R {
        self.inner.lock(|inner| {
            let mut inner = inner.borrow_mut();
            f(&mut inner)
        })
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

            // Power cycle RADIO.
            let r = pac::RADIO;
            r.power().write(|w| w.set_power(false));
            r.power().write(|w| w.set_power(true));

            // Full ESB register init.
            let mut radio = crate::radio::EsbRadio::new(pac::RADIO);
            radio.init(state.config.as_ref().unwrap(), state.addresses.as_ref().unwrap());
            radio.restore_pid_state([state.pid; 8]);

            // Prepare TX buffer.
            let tx_buf = unsafe { &mut *PTX_BUFS.tx.get() };
            let payload_len: usize = 4;
            let dma_off = crate::header::EsbHeader::DMA_OFFSET;
            tx_buf[0] = 0;
            tx_buf[1] = 0;
            tx_buf[dma_off] = payload_len as u8;
            tx_buf[dma_off + 1] = (state.pid << 1) & 0x06;
            let p_off = crate::header::EsbHeader::PAYLOAD_OFFSET;
            tx_buf[p_off] = state.payload_byte;
            tx_buf[p_off + 1] = state.payload_byte;
            tx_buf[p_off + 2] = state.payload_byte;
            tx_buf[p_off + 3] = state.payload_byte;

            // Arm TIMER0 for slot end.
            let t = pac::TIMER0;
            t.events_compare(0).write_value(0);
            t.cc(0).write_value(state.in_slot_match_us);
            t.intenset().write(|w| w.set_compare(0, true));

            // Trigger TX with ACK.
            let dma_ptr = unsafe { tx_buf.as_mut_ptr().add(dma_off) };
            radio.transmit(state.tx_pipe, dma_ptr, true);
            state.tx_count += 1;

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
                        rx_buf.as_mut_ptr().add(crate::header::EsbHeader::DMA_OFFSET)
                    };

                    r.events_ready().write_value(0);
                    compiler_fence(Ordering::Release);
                    r.packetptr().write_value(dma_ptr as u32);
                    r.shorts().modify(|w| w.set_disabled_rxen(false));

                    state.phase = PtxPhase::WaitAck;
                }
                PtxPhase::WaitAck if disabled => {
                    r.events_disabled().write_value(0);

                    let crc_ok = r.crcstatus().read().crcstatus()
                        == pac::radio::vals::Crcstatus::CRCOK;
                    compiler_fence(Ordering::Acquire);

                    if crc_ok {
                        state.ack_ok_count += 1;
                    }

                    state.pid = (state.pid + 1) & 0x03;
                    state.payload_byte = state.payload_byte.wrapping_add(1);

                    // Stop radio.
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

            let chain = state.target_count > 0 && state.tx_count < state.target_count;
            if chain {
                state.return_param.callback_action =
                    raw::MPSL_TIMESLOT_SIGNAL_ACTION_REQUEST as u8;
                state.return_param.params.request.p_next =
                    core::ptr::from_mut(&mut state.request);
            } else {
                state.done = true;
                state.waker.wake();
                state.return_param.callback_action = raw::MPSL_TIMESLOT_SIGNAL_ACTION_END as u8;
            }
            &mut state.return_param as *mut _
        }),

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_IDLE => PTX_STATE.with_inner(|state| {
            state.counters.session_idle += 1;
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
                state.request.params.earliest.priority =
                    raw::MPSL_TIMESLOT_PRIORITY_HIGH as u8;
                state.request.params.earliest.timeout_us =
                    raw::MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US;
                core::ptr::from_ref(&state.request)
            });
            let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
            assert!(ret == 0);
            core::ptr::null_mut()
        }

        raw::MPSL_TIMESLOT_SIGNAL_SESSION_CLOSED => PTX_STATE.with_inner(|state| {
            state.counters.session_closed += 1;
            state.done = true;
            state.waker.wake();
            core::ptr::null_mut()
        }),

        raw::MPSL_TIMESLOT_SIGNAL_OVERSTAYED => {
            panic!("PTX timeslot overstayed");
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
) -> PtxSlotResult {
    let mut session_id: u8 = 0;
    let ret = unsafe {
        raw::mpsl_timeslot_session_open(
            Some(ptx_timeslot_callback),
            (&mut session_id) as *mut _,
        )
    };
    RetVal::from(ret).to_result().unwrap();

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
        state.tx_pipe = tx_pipe;
        state.payload_byte = 0;
        state.request.request_type = raw::MPSL_TIMESLOT_REQ_TYPE_EARLIEST as u8;
        state.request.params.earliest.hfclk = raw::MPSL_TIMESLOT_HFCLK_CFG_NO_GUARANTEE as u8;
        state.request.params.earliest.priority = raw::MPSL_TIMESLOT_PRIORITY_NORMAL as u8;
        state.request.params.earliest.length_us = slot_length_us;
        state.request.params.earliest.timeout_us = 1_000_000;
    });

    let request = PTX_STATE.with_inner(|state| core::ptr::from_ref(&state.request));
    let ret = unsafe { raw::mpsl_timeslot_request(session_id, request) };
    RetVal::from(ret).to_result().unwrap();

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
        RetVal::from(ret).to_result().unwrap();
    }

    PTX_STATE.with_inner(|state| PtxSlotResult {
        counters: state.counters,
        tx_count: state.tx_count,
        ack_ok_count: state.ack_ok_count,
    })
}
