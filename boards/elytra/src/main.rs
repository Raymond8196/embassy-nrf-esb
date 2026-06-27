//! Elytra (nRF52833) right/peripheral-half firmware: real key matrix -> ESB event PTX.
//!
//! Port of `examples/mpsl_3mode_event.rs` onto the Elytra split-keyboard
//! peripheral half. The placeholder 2x2 dongle matrix + on-board SW1 are
//! replaced with the keyboard's real 5 row x 8 col matrix. Each detected key
//! transition triggers one ESB event-PTX send (with cross-attempt retry);
//! an untouched matrix issues zero timeslot requests.
//!
//! Radio pairing and the coexistence profile are kept identical to
//! `mpsl_3mode_event`, so the unmodified `mpsl_3mode_central` (nRF52840 dongle)
//! receives these packets.
//!
//! Flash over SWD:
//!   cargo run --release        # uses the probe-rs runner in .cargo/config.toml

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_futures::select::{select, Either};
use embassy_time::Instant;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State as CdcState};
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_schedule::{LinkTiming, LinkTimingConfig, LinkTimingMode};
use embassy_nrf_esb::mpsl_timeslot::{
    CoexistenceProfile, PtxEventConfig, SignalCounters, open_event_session,
};
use embassy_nrf_esb::transport;

type MyUsbDriver = UsbDriver<'static, &'static SoftwareVbusDetect>;

const LOG_BUF_SIZE: usize = 256;
const RETRY_DELAY_US: u64 = 250;
const RIGHT_DEVICE_ID: u8 = 0;
const ENABLE_FIRST_ATTEMPT_ALIGNMENT: bool = true;
const ENABLE_HINT_RETRY_WAIT: bool = true;
const MIN_HINT_RETRY_GAP_US: u32 = 500;
const LINK_TIMING_CONFIG: LinkTimingConfig = LinkTimingConfig {
    hint_valid_us: 200_000,
    max_wait_us: 2_000,
    window_guard_us: 400,
    miss_limit: 4,
};

static LOG_CHANNEL: Channel<CriticalSectionRawMutex, heapless::Vec<u8, LOG_BUF_SIZE>, 4> =
    Channel::new();

// ---- Elytra right-hand key matrix event source (5 rows x 8 cols, col2row) ----
//
// Columns are driven outputs (idle low, pulsed high one at a time); rows are
// pulled-down inputs that read high when a pressed key bridges the active
// column. Pins mirror the Elytra peripheral half (HaoboGu/utb `feat/update`,
// firmware/src/peripheral.rs). Scanning never requests a timeslot; only a real
// press/release transition updates a compact matrix snapshot. The ESB sender
// retries the latest snapshot instead of serializing individual edges.

const MATRIX_ROWS: usize = 5;
const MATRIX_COLS: usize = 8;
const SCAN_INTERVAL_MS: u64 = 2;
/// While a key is held, the matrix doesn't change so no event fires. Send a
/// keepalive snapshot at this interval so the left's link-loss watchdog (500ms)
/// sees fresh data and doesn't release the held key mid-hold (which would stop
/// OS auto-repeat). Keep well under the 500ms watchdog so jitter (bounded_wait
/// + retries under interference) can't close the margin — 100ms gives ~5x.
const KEY_HOLD_KEEPALIVE_MS: u64 = 100;
const COL_SETTLE_US: u64 = 5;

const SNAPSHOT_MSG: u8 = 0x53;
const SNAPSHOT_PAYLOAD_LEN: usize = 1 + MATRIX_ROWS;

static MATRIX_ROWS_STATE: [AtomicU8; MATRIX_ROWS] = [
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
];
static MATRIX_DIRTY: AtomicBool = AtomicBool::new(false);
static MATRIX_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[embassy_executor::task]
async fn matrix_scan_task(
    mut cols: [Output<'static>; MATRIX_COLS],
    rows: [Input<'static>; MATRIX_ROWS],
) {
    let mut state = [[false; MATRIX_COLS]; MATRIX_ROWS];
    let mut raw = [0u8; MATRIX_COLS];
    let mut tick: u32 = 0;
    loop {
        for (c, col) in cols.iter_mut().enumerate() {
            col.set_high();
            embassy_time::Timer::after_micros(COL_SETTLE_US).await;
            let mut mask = 0u8;
            for (r, row) in rows.iter().enumerate() {
                let pressed = row.is_high();
                if pressed {
                    mask |= 1 << r;
                }
                if pressed != state[r][c] {
                    state[r][c] = pressed;
                    defmt::info!("edge r={} c={} pr={}", r, c, pressed as u8);
                }
            }
            raw[c] = mask;
            col.set_low();
        }

        let mut rows = [0u8; MATRIX_ROWS];
        for r in 0..MATRIX_ROWS {
            let mut row_mask = 0u8;
            for c in 0..MATRIX_COLS {
                if state[r][c] {
                    row_mask |= 1 << c;
                }
            }
            rows[r] = row_mask;
        }
        let mut changed = false;
        for r in 0..MATRIX_ROWS {
            if MATRIX_ROWS_STATE[r].swap(rows[r], Ordering::AcqRel) != rows[r] {
                changed = true;
            }
        }
        if changed {
            MATRIX_DIRTY.store(true, Ordering::Release);
            MATRIX_CHANGED.signal(());
        }

        tick = tick.wrapping_add(1);
        if tick % 250 == 0 {
            let mut dbg = [0u8; LOG_BUF_SIZE];
            let mut w = WriteBuf::new(&mut dbg);
            let _ = write!(
                w,
                "RAW c0={} c1={} c2={} c3={} c4={} c5={} c6={} c7={} (bit r0=1 r1=2 r2=4 r3=8 r4=16)\r\n",
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            );
            let n = w.pos;
            log(&dbg[..n]);
        }

        embassy_time::Timer::after_millis(SCAN_INTERVAL_MS).await;
    }
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    EGU0_SWI0 => nrf_mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_mpsl::ClockInterruptHandler;
    RADIO => nrf_mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_mpsl::HighPrioInterruptHandler;
});

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await
}

#[embassy_executor::task]
async fn cdc_logger_task(mut cdc: CdcAcmClass<'static, MyUsbDriver>) {
    cdc.wait_connection().await;
    let _ = cdc.write_packet(b"[EVENT] CDC connected\r\n").await;
    loop {
        let msg = LOG_CHANNEL.receive().await;
        for chunk in msg.chunks(64) {
            let _ = cdc.write_packet(chunk).await;
        }
    }
}

fn log(data: &[u8]) {
    let v = heapless::Vec::from_slice(data);
    if let Ok(v) = v {
        let _ = LOG_CHANNEL.try_send(v);
    }
}

struct WriteBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> WriteBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl core::fmt::Write for WriteBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = (self.pos + bytes.len()).min(self.buf.len());
        let count = end - self.pos;
        self.buf[self.pos..end].copy_from_slice(&bytes[..count]);
        self.pos = end;
        Ok(())
    }
}

fn link_mode_id(mode: LinkTimingMode) -> u8 {
    match mode {
        LinkTimingMode::Unsynced => 0,
        LinkTimingMode::Synced => 1,
        LinkTimingMode::Degraded => 2,
    }
}

// ---- Main ----

const PROFILE: CoexistenceProfile = CoexistenceProfile::NordicExtend;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Elytra power path: enable the high-voltage REG0 at 3V3 and the DC/DC
    // converters, matching the vendor firmware (utb firmware/src/peripheral.rs).
    let mut nrf_config = embassy_nrf::config::Config::default();
    nrf_config.dcdc.reg0_voltage = Some(embassy_nrf::config::Reg0Voltage::_3V3);
    nrf_config.dcdc.reg1 = true;
    let p = embassy_nrf::init(nrf_config);

    let lfclk_cfg = raw::mpsl_clock_lfclk_cfg_t {
        source: raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: 16,
        rc_temp_ctiv: 2,
        accuracy_ppm: 500,
        skip_wait_lfclk_started: false,
    };

    let mpsl_p = Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
    static SESSION_MEM: StaticCell<SessionMem<1>> = StaticCell::new();
    let session_mem = SESSION_MEM.init(SessionMem::new());
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(
        MultiprotocolServiceLayer::with_timeslots::<typelevel::EGU0_SWI0, _, 1>(
            mpsl_p,
            Irqs,
            lfclk_cfg,
            session_mem,
        )
        .unwrap(),
    );
    spawner.spawn(mpsl_task(mpsl).unwrap());
    defmt::info!("Right ESB PTX only; BLE disabled on this half");

    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);
    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0004);
    usb_config.manufacturer = Some("Elytra");
    usb_config.product = Some("Elytra Event PTX");
    usb_config.max_power = 100;
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static CDC_STATE: StaticCell<CdcState<'static>> = StaticCell::new();
    let mut builder = embassy_usb::Builder::new(
        driver,
        usb_config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );
    let cdc_class = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), 64);
    let usb_dev = builder.build();
    spawner.spawn(usb_task(usb_dev).unwrap());
    spawner.spawn(cdc_logger_task(cdc_class).unwrap());

    // Real Elytra right/peripheral-half key matrix from HaoboGu/utb
    // `feat/update` firmware/src/peripheral.rs: 8 columns drive,
    // 5 rows read with pulldown.
    let cols: [Output<'static>; MATRIX_COLS] = [
        Output::new(p.P0_00, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_01, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_30, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_29, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_03, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_25, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_14, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_22, Level::Low, OutputDrive::Standard),
    ];
    let rows: [Input<'static>; MATRIX_ROWS] = [
        Input::new(p.P1_08, Pull::Down),
        Input::new(p.P0_06, Pull::Down),
        Input::new(p.P0_05, Pull::Down),
        Input::new(p.P0_26, Pull::Down),
        Input::new(p.P1_03, Pull::Down),
    ];
    spawner.spawn(matrix_scan_task(cols, rows).unwrap());

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    let event_cfg = PtxEventConfig::for_profile(PROFILE);
    let mut startup_buf = [0u8; LOG_BUF_SIZE];
    let mut startup = WriteBuf::new(&mut startup_buf);
    let _ = write!(
        startup,
        "[START] role=event profile={:?} cfg={}/{} pipe={} ack_to={} retries={} req_to={} retry_hi={} align={} retry_wait={}\r\n",
        PROFILE,
        event_cfg.slot_length_us,
        event_cfg.in_slot_match_us,
        event_cfg.pipe,
        event_cfg.ack_timeout_us,
        event_cfg.max_retries,
        event_cfg.request.timeout_us,
        event_cfg.request.retry_blocked_at_high_priority as u8,
        ENABLE_FIRST_ATTEMPT_ALIGNMENT as u8,
        ENABLE_HINT_RETRY_WAIT as u8,
    );
    let startup_len = startup.pos;
    log(&startup_buf[..startup_len]);

    let mut session = match open_event_session(&mpsl, &esb_cfg, &esb_addr, event_cfg) {
        Ok(s) => s,
        Err(e) => {
            defmt::error!("open_event_session: {:?}", e);
            core::future::pending::<()>().await;
            unreachable!()
        }
    };

    defmt::info!("Event PTX session opened");
    log(b"[EVENT] session opened\r\n");

    let mut event_count: u32 = 0;
    let mut total_acked: u32 = 0;
    let mut total_sends: u32 = 0;
    let mut sequence: u8 = 0;
    let mut link_timing = LinkTiming::new();

    loop {
        while !MATRIX_DIRTY.swap(false, Ordering::AcqRel) {
            // Held keys don't change the matrix, so no event fires — send a
            // keepalive snapshot periodically so the left's link-loss watchdog
            // doesn't release the held key (which kills OS auto-repeat).
            if MATRIX_ROWS_STATE.iter().any(|r| r.load(Ordering::Acquire) != 0) {
                match select(
                    MATRIX_CHANGED.wait(),
                    embassy_time::Timer::after_millis(KEY_HOLD_KEEPALIVE_MS),
                )
                .await
                {
                    Either::First(()) => {} // change: scan already set MATRIX_DIRTY
                    Either::Second(()) => MATRIX_DIRTY.store(true, Ordering::Release),
                }
            } else {
                MATRIX_CHANGED.wait().await;
            }
        }

        event_count += 1;
        let event_since_us = Instant::now().as_micros();
        let snapshot_seq = sequence;
        sequence = sequence.wrapping_add(1);
        let mut report = [0u8; SNAPSHOT_PAYLOAD_LEN];
        report[0] = SNAPSHOT_MSG;
        for r in 0..MATRIX_ROWS {
            report[1 + r] = MATRIX_ROWS_STATE[r].load(Ordering::Acquire);
        }
        let mut frame = [0u8; 16];
        let mut attempts: u32 = 0;
        let mut event_counters = SignalCounters::ZERO;
        let mut wait_us: u32 = 0;
        let mut fallback_count: u32 = 0;
        let mut radio_ack_count: u32 = 0;
        let mut transport_ack_count: u32 = 0;
        let mut last_ack_dev: u8 = 0xff;
        let mut last_ack_seq: u8 = 0xff;
        let mut superseded = false;

        if ENABLE_FIRST_ATTEMPT_ALIGNMENT {
            let wait = link_timing.bounded_wait(Instant::now().as_micros(), LINK_TIMING_CONFIG);
            if wait.fallback.is_some() {
                fallback_count = fallback_count.saturating_add(1);
            } else if wait.wait_us > 0 {
                wait_us = wait_us.saturating_add(wait.wait_us);
                embassy_time::Timer::after_micros(wait.wait_us as u64).await;
            }
        }

        loop {
            let flags = if attempts == 0 {
                0
            } else {
                transport::FLAG_RETRANSMIT
            };
            let frame_len =
                transport::encode_frame(RIGHT_DEVICE_ID, snapshot_seq, flags, &report, &mut frame)
                    .unwrap();

            attempts = attempts.saturating_add(1);
            total_sends = total_sends.saturating_add(1);

            let result = match session.send(&frame[..frame_len]).await {
                Ok(r) => r,
                Err(e) => {
                    defmt::warn!("send error: {:?}", e);
                    let mut err_buf = [0u8; LOG_BUF_SIZE];
                    let mut ew = WriteBuf::new(&mut err_buf);
                    let _ = write!(
                        ew,
                        "[ERR] send seq={} att={} err={:?} rows={},{},{},{},{}\r\n",
                        snapshot_seq,
                        attempts,
                        e,
                        report[1],
                        report[2],
                        report[3],
                        report[4],
                        report[5],
                    );
                    let err_len = ew.pos;
                    log(&err_buf[..err_len]);
                    fallback_count = fallback_count.saturating_add(1);
                    embassy_time::Timer::after_micros(RETRY_DELAY_US).await;
                    if MATRIX_DIRTY.load(Ordering::Acquire) {
                        superseded = true;
                        break;
                    }
                    continue;
                }
            };

            event_counters = event_counters.saturating_add(result.counters);

            if result.ack_ok {
                radio_ack_count = radio_ack_count.saturating_add(1);
                if let Some(hint) = result.schedule_hint {
                    link_timing.observe_hint(Instant::now().as_micros(), hint);
                }
                if let Some(extension) = result.ack_extension {
                    if let Ok(ack) = transport::decode_transport_ack(extension.as_slice()) {
                        transport_ack_count = transport_ack_count.saturating_add(1);
                        last_ack_dev = ack.device_id;
                        last_ack_seq = ack.sequence;
                        if ack.matches(RIGHT_DEVICE_ID, snapshot_seq) {
                            break;
                        }
                    }
                }
            }

            link_timing.observe_miss(LINK_TIMING_CONFIG);
            if MATRIX_DIRTY.load(Ordering::Acquire) {
                superseded = true;
                break;
            }

            if ENABLE_HINT_RETRY_WAIT {
                let wait = link_timing.bounded_wait(Instant::now().as_micros(), LINK_TIMING_CONFIG);
                if wait.fallback.is_some() {
                    fallback_count = fallback_count.saturating_add(1);
                    embassy_time::Timer::after_micros(RETRY_DELAY_US).await;
                } else {
                    let actual_wait_us = wait.wait_us.max(MIN_HINT_RETRY_GAP_US);
                    wait_us = wait_us.saturating_add(actual_wait_us);
                    embassy_time::Timer::after_micros(actual_wait_us as u64).await;
                }
            } else {
                fallback_count = fallback_count.saturating_add(1);
                embassy_time::Timer::after_micros(RETRY_DELAY_US).await;
            }

            if MATRIX_DIRTY.load(Ordering::Acquire) {
                superseded = true;
                break;
            }
        }

        let acked = !superseded;
        if acked {
            total_acked = total_acked.saturating_add(1);
        }

        let now_us = Instant::now().as_micros();
        let latency_us = now_us.saturating_sub(event_since_us);
        let link = link_timing.snapshot(now_us);
        defmt::info!(
            "snap seq={} ok={} att={} rack={} tack={} ack={}:{} sup={} lat={} rows={},{},{},{},{}",
            snapshot_seq,
            acked as u8,
            attempts,
            radio_ack_count,
            transport_ack_count,
            last_ack_dev,
            last_ack_seq,
            superseded as u8,
            latency_us,
            report[1],
            report[2],
            report[3],
            report[4],
            report[5],
        );

        let mut buf = [0u8; LOG_BUF_SIZE];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "snap={} seq={} rows={},{},{},{},{} ok={} att={} rack={} tack={} ack={}:{} drop={} s={} t0={} rd={} bk={} cn={} dt={} si={} sc={} ov={} iv={} lat={} sync={} wait={} fb={} lock={} miss={} age={}\r\n",
            event_count,
            snapshot_seq,
            report[1],
            report[2],
            report[3],
            report[4],
            report[5],
            acked,
            attempts,
            radio_ack_count,
            transport_ack_count,
            last_ack_dev,
            last_ack_seq,
            superseded,
            event_counters.start,
            event_counters.timer0,
            event_counters.radio,
            event_counters.blocked,
            event_counters.cancelled,
            event_counters.radio_disable_timeout,
            event_counters.session_idle,
            event_counters.session_closed,
            event_counters.overstayed,
            event_counters.invalid_return,
            latency_us,
            link_mode_id(link.mode),
            wait_us,
            fallback_count,
            link.lock_count,
            link.miss_streak,
            link.hint_age_us,
        );
        let log_len = w.pos;
        log(&buf[..log_len]);

        if event_count % 20 == 0 {
            let mut w2 = WriteBuf::new(&mut buf);
            let _ = write!(
                w2,
                "[CUM] events={} sends={} acked={} rate={:.4}\r\n",
                event_count,
                total_sends,
                total_acked,
                total_acked as f64 / event_count as f64,
            );
            let cum_len = w2.pos;
            log(&buf[..cum_len]);
        }
    }
}
