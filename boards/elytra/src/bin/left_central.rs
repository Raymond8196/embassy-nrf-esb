//! Elytra left/central firmware: left matrix + ESB PRX from right half + BLE HID.

#![no_std]
#![no_main]

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};

use bt_hci::cmd::SyncCmd;
use bt_hci::cmd::controller_baseband::SetEventMask;
use bt_hci::cmd::le::{
    LeLongTermKeyRequestNegativeReply, LeLongTermKeyRequestReply, LeRand, LeSetAdvData,
    LeSetAdvEnable, LeSetAdvParams, LeSetEventMask,
};
use bt_hci::param::{
    AdvChannelMap, AdvFilterPolicy, AdvKind, BdAddr, ConnHandle, EventMask, LeEventMask,
};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::{bind_interrupts, peripherals, rng, usb};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use nrf_sdc::vendor::ZephyrWriteBdAddr;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_timeslot::{
    CoexistenceProfile, PrxAckExtension, PrxSlotConfig, open_parked_prx_session_with_ack_extension,
};
use embassy_nrf_esb::transport::{
    SequenceTracker, StaticBindingTable, TRANSPORT_ACK_LEN, TransportAck, accept_bound_frame,
    decode_frame, encode_transport_ack,
};

type Rng = rng::Rng<'static, embassy_nrf::mode::Blocking>;

const ROWS: usize = 5;
const COLS: usize = 7;
const RIGHT_COLS: usize = 8;
const SCAN_INTERVAL_MS: u64 = 2;
const COL_SETTLE_US: u64 = 5;
const HID_REPORT_HANDLE: u16 = 12;
const HID_REPORT_CCCD_HANDLE: u16 = 14;
const CONN_NONE: u16 = 0xffff;
const ATT_VALUE_CHUNK: usize = 22;
const SMP_PAIRING_REQUEST: u8 = 0x01;
const SMP_PAIRING_RESPONSE: u8 = 0x02;
const SMP_PAIRING_CONFIRM: u8 = 0x03;
const SMP_PAIRING_RANDOM: u8 = 0x04;
const SMP_PAIRING_FAILED: u8 = 0x05;
const SMP_ENCRYPTION_INFORMATION: u8 = 0x06;
const SMP_MASTER_IDENTIFICATION: u8 = 0x07;
const SMP_SECURITY_REQUEST: u8 = 0x0b;
const SMP_AUTH_BONDING: u8 = 0x01;
const SMP_KEYDIST_ENCKEY: u8 = 0x01;
const SMP_FIXED_EDIV: u16 = 0x4553;
const SMP_FIXED_RAND: [u8; 8] = *b"ELYTRAES";
const RIGHT_KEY_BASE: u8 = 64;
const SNAPSHOT_MSG: u8 = 0x53;
const SNAPSHOT_PAYLOAD_LEN: usize = 1 + ROWS;
const TOTAL_COLS: usize = COLS + RIGHT_COLS;
const TOTAL_KEYS: usize = ROWS * TOTAL_COLS;
const RIGHT_DEVICE_ID: u8 = 0;

static CONN_HANDLE: AtomicU16 = AtomicU16::new(CONN_NONE);
static HID_NOTIFY_ENABLED: AtomicBool = AtomicBool::new(false);
static HID_EVENTS: Channel<CriticalSectionRawMutex, KeyEvent, 64> = Channel::new();
static RIGHT_ROWS_STATE: [AtomicU8; ROWS] = [
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
];
static RIGHT_DIRTY: AtomicBool = AtomicBool::new(false);
static RIGHT_SEQUENCE: AtomicU8 = AtomicU8::new(0);
static RIGHT_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Timestamp (embassy ticks) of the last received right-half packet.
/// `hid_task` uses this to detect ESB link loss and release stale keys.
static RIGHT_LAST_SEEN_MS: AtomicU32 = AtomicU32::new(0);
/// Whether the PRX (right-half ESB) session is active. Set by `prx_task`;
/// read by `hid_task` to know whether link-loss timeout applies.
static PRX_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Signal to force an all-keys-up HID report (BLE disconnect, link loss, etc.).
static HID_CLEAR_REQUESTED: AtomicBool = AtomicBool::new(false);
static SMP_STATE: Mutex<CriticalSectionRawMutex, RefCell<SmpState>> =
    Mutex::new(RefCell::new(SmpState::new()));

// BLE-INACTIVE radio-notification signal. Set by the priority-0 callback when
// a BLE connection event ends; prx_task waits on it to request an ESB RX window.
static BLE_INACTIVE_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[derive(Clone, Copy)]
struct KeyEvent {
    key_id: u8,
    pressed: bool,
}

#[derive(Clone, Copy)]
struct HidUsage {
    modifier: u8,
    key: u8,
}

#[derive(Clone, Copy)]
enum SmpStage {
    Idle,
    WaitConfirm,
    WaitRandom,
    WaitEncrypted,
}

struct SmpState {
    stage: SmpStage,
    local_addr: [u8; 6],
    peer_addr: [u8; 6],
    peer_addr_type: u8,
    preq: [u8; 7],
    pres: [u8; 7],
    srand: [u8; 16],
    mconfirm: [u8; 16],
    stk: [u8; 16],
    ltk: [u8; 16],
}

impl SmpState {
    const fn new() -> Self {
        Self {
            stage: SmpStage::Idle,
            local_addr: [0; 6],
            peer_addr: [0; 6],
            peer_addr_type: 0,
            preq: [0; 7],
            pres: [0; 7],
            srand: [0; 16],
            mconfirm: [0; 16],
            stk: [0; 16],
            ltk: [0; 16],
        }
    }

    fn reset_pairing(&mut self) {
        self.stage = SmpStage::Idle;
        self.preq = [0; 7];
        self.pres = [0; 7];
        self.srand = [0; 16];
        self.mconfirm = [0; 16];
        self.stk = [0; 16];
    }
}

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<peripherals::RNG>;
    EGU0_SWI0 => nrf_mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_mpsl::ClockInterruptHandler, usb::vbus_detect::InterruptHandler;
    RADIO => nrf_mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_mpsl::HighPrioInterruptHandler;
});

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

#[embassy_executor::task]
async fn hfclk_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    let _hfclk = loop {
        match mpsl.request_hfclk().await {
            Ok(guard) => break guard,
            Err(e) => {
                defmt::warn!("HFCLK request failed: {:?}, retrying in 500ms", e);
                Timer::after_millis(500).await;
            }
        }
    };
    core::future::pending().await
}

/// Priority-0 MPSL radio-notification callback. Fires at the end of each BLE
/// radio event (INACTIVE). Must stay tiny: just signal the PRX task.
unsafe extern "C" fn radio_notification_cb(source: raw::mpsl_radio_notification_source_t) {
    if source == raw::MPSL_RADIO_NOTIFICATION_SOURCE_INACTIVE {
        BLE_INACTIVE_SIGNAL.signal(());
    }
}

#[embassy_executor::task]
async fn sdc_task(
    sdc: &'static SoftdeviceController<'static>,
    mpsl: &'static MultiprotocolServiceLayer<'static>,
) -> ! {
    let mut evt_buf = [0u8; sdc::raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
    loop {
        match sdc.hci_get(&mut evt_buf).await {
            Ok(bt_hci::PacketKind::AclData) => handle_acl(sdc, mpsl, &evt_buf).await,
            Ok(bt_hci::PacketKind::Event) => {
                if handle_hci_event(sdc, &evt_buf).await {
                    if let Err(e) = LeSetAdvEnable::new(true).exec(sdc).await {
                        defmt::warn!("BLE adv restart failed: {:?}", e);
                    } else {
                        defmt::info!("BLE advertising restarted");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => defmt::warn!("sdc error: {:?}", e),
        }
    }
}

#[embassy_executor::task]
async fn matrix_scan_task(mut cols: [Output<'static>; COLS], rows: [Input<'static>; ROWS]) {
    let mut state = [[false; COLS]; ROWS];
    loop {
        for (c, col) in cols.iter_mut().enumerate() {
            col.set_high();
            embassy_time::Timer::after_micros(COL_SETTLE_US).await;
            for (r, row) in rows.iter().enumerate() {
                let pressed = row.is_high();
                if pressed != state[r][c] {
                    state[r][c] = pressed;
                    let key_id = (r * COLS + c) as u8;
                    defmt::info!("left edge r={} c={} pr={}", r, c, pressed as u8);
                    let _ = HID_EVENTS.try_send(KeyEvent { key_id, pressed });
                }
            }
            col.set_low();
        }
        embassy_time::Timer::after_millis(SCAN_INTERVAL_MS).await;
    }
}

/// Board-level split transport policy: ACK a validated right-hand frame only
/// after the generic PRX event queue accepted it. This runs in the MPSL RADIO
/// callback, so it only performs bounded parsing and fixed-size copies.
fn right_transport_ack_extension(pipe: u8, payload: &[u8], queued: bool) -> PrxAckExtension {
    if !queued || pipe != 1 {
        return PrxAckExtension::EMPTY;
    }

    let Ok((header, _)) = decode_frame(payload) else {
        return PrxAckExtension::EMPTY;
    };
    if header.device_id != RIGHT_DEVICE_ID {
        return PrxAckExtension::EMPTY;
    }

    let mut bytes = [0u8; TRANSPORT_ACK_LEN];
    if encode_transport_ack(
        TransportAck::new(header.device_id, header.sequence),
        &mut bytes,
    )
    .is_err()
    {
        return PrxAckExtension::EMPTY;
    }

    PrxAckExtension::from_slice(&bytes).unwrap_or(PrxAckExtension::EMPTY)
}

#[embassy_executor::task]
async fn prx_task(mpsl: &'static MultiprotocolServiceLayer<'static>) {
    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();
    let cfg = PrxSlotConfig::for_profile(CoexistenceProfile::NordicExtend);

    // Parked PRX: opens a single RX window on each BLE-INACTIVE notification
    // instead of continuous chaining. Cuts idle current from ~4-5mA (continuous
    // RX) to the BLE-event-rate cost only (~200µA). Hardware-verified: right-half
    // packets arrive with no drops/dupes; no self-trigger loop.
    let mut session = loop {
        match open_parked_prx_session_with_ack_extension(
            mpsl,
            &esb_cfg,
            &esb_addr,
            cfg,
            Some(right_transport_ack_extension),
        ) {
            Ok(s) => break s,
            Err(e) => {
                defmt::warn!("parked PRX open failed: {:?}, retrying in 1s", e);
                Timer::after_secs(1).await;
            }
        }
    };
    PRX_ACTIVE.store(true, Ordering::Release);
    let bindings = StaticBindingTable::<8>::from_pipe_entries([
        None,
        Some(RIGHT_DEVICE_ID),
        None,
        None,
        None,
        None,
        None,
        None,
    ]);
    let mut tracker = SequenceTracker::<1>::new();
    defmt::info!("ESB parked PRX session opened (BLE-gap triggered)");

    loop {
        BLE_INACTIVE_SIGNAL.wait().await;

        // Drain events from the previous slot before requesting the next window.
        // try_next_event is non-blocking; next_event would wait and could stall
        // on a slot that received nothing.
        while let Some(ev) = session.try_next_event() {
            let frame = &ev.payload[..ev.len as usize];
            let (header, payload) = match accept_bound_frame(
                &bindings,
                &mut tracker,
                ev.pipe,
                frame,
            ) {
                Ok(Some(frame)) => frame,
                Ok(None) => continue,
                Err(_) => {
                    defmt::warn!(
                        "right frame invalid pipe={} len={} b0={} b1={} b2={} b3={} b4={} b5={} b6={} b7={}",
                        ev.pipe,
                        ev.len,
                        ev.payload[0],
                        ev.payload[1],
                        ev.payload[2],
                        ev.payload[3],
                        ev.payload[4],
                        ev.payload[5],
                        ev.payload[6],
                        ev.payload[7]
                    );
                    continue;
                }
            };

            RIGHT_LAST_SEEN_MS.store(
                embassy_time::Instant::now().as_millis() as u32,
                Ordering::Release,
            );

            if payload.len() == SNAPSHOT_PAYLOAD_LEN && payload[0] == SNAPSHOT_MSG {
                let mut changed_count = 0u8;
                for r in 0..ROWS {
                    let next = payload[1 + r];
                    let previous = RIGHT_ROWS_STATE[r].swap(next, Ordering::AcqRel);
                    if previous != next {
                        changed_count =
                            changed_count.saturating_add((previous ^ next).count_ones() as u8);
                    }
                }
                if changed_count != 0 {
                    RIGHT_SEQUENCE.store(header.sequence, Ordering::Release);
                    RIGHT_DIRTY.store(true, Ordering::Release);
                    RIGHT_CHANGED.signal(());
                }
                defmt::info!(
                    "right snapshot pipe={} dev={} seq={} rows={},{},{},{},{} chg={}",
                    ev.pipe,
                    header.device_id,
                    header.sequence,
                    payload[1],
                    payload[2],
                    payload[3],
                    payload[4],
                    payload[5],
                    changed_count
                );
                continue;
            }

            if payload.len() != 2 {
                defmt::warn!("right event invalid payload len={}", payload.len());
                continue;
            }
            let key_id = payload[0];
            let pressed = payload[1] != 0;
            defmt::info!(
                "right event pipe={} dev={} key={} pr={} seq={}",
                ev.pipe,
                header.device_id,
                key_id,
                pressed as u8,
                header.sequence
            );
            if key_id < (ROWS * RIGHT_COLS) as u8 {
                let _ = HID_EVENTS.try_send(KeyEvent {
                    key_id: key_id + RIGHT_KEY_BASE,
                    pressed,
                });
            } else {
                defmt::warn!("right event invalid key={} pr={}", key_id, pressed as u8);
            }
        }

        match session.request_window() {
            Ok(_) => {}
            Err(e) => {
                defmt::warn!("request_window: {:?}", e);
            }
        }
    }
}

fn apply_right_snapshot(active: &mut [bool; TOTAL_KEYS]) {
    for row in 0..ROWS {
        let mask = RIGHT_ROWS_STATE[row].load(Ordering::Acquire);
        for col in 0..RIGHT_COLS {
            let pos = row * TOTAL_COLS + COLS + col;
            active[pos] = mask & (1 << col) != 0;
        }
    }
}

/// Clear all right-half key state and flag the HID task to send an updated report.
fn clear_right_keys() {
    for row in 0..ROWS {
        RIGHT_ROWS_STATE[row].store(0, Ordering::Release);
    }
    RIGHT_DIRTY.store(true, Ordering::Release);
    RIGHT_CHANGED.signal(());
}

/// Request the HID task to send an all-keys-up report on the next iteration.
/// Called on mode switch, panic recovery, or other forced key-release paths.
#[allow(dead_code)]
fn request_hid_clear() {
    HID_CLEAR_REQUESTED.store(true, Ordering::Release);
}

#[embassy_executor::task]
async fn hid_task(sdc: &'static SoftdeviceController<'static>) {
    let mut active = [false; TOTAL_KEYS];
    /// ESB link-loss timeout: if no right-half frame in this duration, release
    /// all right-half keys to prevent stuck keys. Right half sends snapshots
    /// every ~2ms scan; 500ms covers worst-case BLE-preempted gaps.
    const LINK_LOSS_TIMEOUT_MS: u32 = 500;

    loop {
        let mut changed = false;
        let mut right_changed = false;

        // Check for explicit HID clear request (BLE disconnect, etc.).
        if HID_CLEAR_REQUESTED.swap(false, Ordering::AcqRel) {
            // Clear all keys.
            active = [false; TOTAL_KEYS];
            changed = true;
        }

        // ESB link-loss watchdog: if PRX is active but no right-half data
        // has arrived within the timeout, clear right-half key state.
        if PRX_ACTIVE.load(Ordering::Acquire) {
            let last_seen = RIGHT_LAST_SEEN_MS.load(Ordering::Acquire);
            if last_seen > 0 {
                let now = embassy_time::Instant::now().as_millis() as u32;
                if now.saturating_sub(last_seen) > LINK_LOSS_TIMEOUT_MS {
                    let any_right_held = RIGHT_ROWS_STATE
                        .iter()
                        .any(|r| r.load(Ordering::Acquire) != 0);
                    if any_right_held {
                        clear_right_keys();
                        defmt::warn!(
                            "ESB link-loss: no right-half data for {}ms, clearing keys",
                            now.saturating_sub(last_seen)
                        );
                    }
                }
            }
        }

        if RIGHT_DIRTY.swap(false, Ordering::AcqRel) {
            apply_right_snapshot(&mut active);
            changed = true;
            right_changed = true;
        }

        if !changed {
            match select(RIGHT_CHANGED.wait(), HID_EVENTS.receive()).await {
                Either::First(()) => {}
                Either::Second(ev) => {
                    if let Some(pos) = key_id_to_pos(ev.key_id)
                        && active[pos] != ev.pressed
                    {
                        active[pos] = ev.pressed;
                        changed = true;
                    }
                }
            }

            if RIGHT_DIRTY.swap(false, Ordering::AcqRel) {
                apply_right_snapshot(&mut active);
                changed = true;
                right_changed = true;
            }
        }

        while let Ok(ev) = HID_EVENTS.try_receive() {
            if let Some(pos) = key_id_to_pos(ev.key_id) {
                if active[pos] != ev.pressed {
                    active[pos] = ev.pressed;
                    changed = true;
                }
            }
        }

        if !changed {
            continue;
        }

        let mut modifiers = 0u8;
        let mut pressed = [0u8; 6];
        for row in 0..ROWS {
            for col in 0..TOTAL_COLS {
                if !active[row * TOTAL_COLS + col] {
                    continue;
                }
                let Some(hid) = hid_for_pos(row, col) else {
                    continue;
                };
                if hid.modifier != 0 {
                    modifiers |= hid.modifier;
                }
                if hid.key != 0
                    && !pressed.contains(&hid.key)
                    && let Some(slot) = pressed.iter_mut().find(|k| **k == 0)
                {
                    *slot = hid.key;
                }
            }
        }

        let mut report = [0u8; 8];
        report[0] = modifiers;
        report[2..8].copy_from_slice(&pressed);
        if right_changed {
            defmt::info!(
                "right hid seq={} mod={} keys={},{},{},{},{},{}",
                RIGHT_SEQUENCE.load(Ordering::Acquire),
                report[0],
                report[2],
                report[3],
                report[4],
                report[5],
                report[6],
                report[7],
            );
        }
        notify_hid(sdc, &report);
    }
}

fn key_id_to_pos(key_id: u8) -> Option<usize> {
    if key_id >= RIGHT_KEY_BASE {
        let right_id = key_id - RIGHT_KEY_BASE;
        if right_id >= (ROWS * RIGHT_COLS) as u8 {
            return None;
        }
        let row = right_id as usize / RIGHT_COLS;
        let col = right_id as usize % RIGHT_COLS + COLS;
        Some(row * TOTAL_COLS + col)
    } else {
        if key_id >= (ROWS * COLS) as u8 {
            return None;
        }
        let row = key_id as usize / COLS;
        let col = key_id as usize % COLS;
        Some(row * TOTAL_COLS + col)
    }
}

const HID_NO: u8 = 0x00;
const HID_A: u8 = 0x04;
const HID_B: u8 = 0x05;
const HID_C: u8 = 0x06;
const HID_D: u8 = 0x07;
const HID_E: u8 = 0x08;
const HID_F: u8 = 0x09;
const HID_G: u8 = 0x0a;
const HID_H: u8 = 0x0b;
const HID_I: u8 = 0x0c;
const HID_J: u8 = 0x0d;
const HID_K: u8 = 0x0e;
const HID_L: u8 = 0x0f;
const HID_M: u8 = 0x10;
const HID_N: u8 = 0x11;
const HID_O: u8 = 0x12;
const HID_P: u8 = 0x13;
const HID_Q: u8 = 0x14;
const HID_R: u8 = 0x15;
const HID_S: u8 = 0x16;
const HID_T: u8 = 0x17;
const HID_U: u8 = 0x18;
const HID_V: u8 = 0x19;
const HID_W: u8 = 0x1a;
const HID_X: u8 = 0x1b;
const HID_Y: u8 = 0x1c;
const HID_Z: u8 = 0x1d;
const HID_1: u8 = 0x1e;
const HID_2: u8 = 0x1f;
const HID_3: u8 = 0x20;
const HID_4: u8 = 0x21;
const HID_5: u8 = 0x22;
const HID_6: u8 = 0x23;
const HID_7: u8 = 0x24;
const HID_8: u8 = 0x25;
const HID_9: u8 = 0x26;
const HID_0: u8 = 0x27;
const HID_ENTER: u8 = 0x28;
const HID_ESC: u8 = 0x29;
const HID_BACKSPACE: u8 = 0x2a;
const HID_TAB: u8 = 0x2b;
const HID_SPACE: u8 = 0x2c;
const HID_MINUS: u8 = 0x2d;
const HID_EQUAL: u8 = 0x2e;
const HID_LEFT_BRACKET: u8 = 0x2f;
const HID_RIGHT_BRACKET: u8 = 0x30;
const HID_BACKSLASH: u8 = 0x31;
const HID_SEMICOLON: u8 = 0x33;
const HID_QUOTE: u8 = 0x34;
const HID_COMMA: u8 = 0x36;
const HID_DOT: u8 = 0x37;
const HID_SLASH: u8 = 0x38;
const HID_CAPS_LOCK: u8 = 0x39;
const HID_RIGHT: u8 = 0x4f;
const HID_LEFT: u8 = 0x50;
const HID_DOWN: u8 = 0x51;
const HID_UP: u8 = 0x52;
const HID_MOD_LCTRL: u8 = 0xe0;
const HID_MOD_LSHIFT: u8 = 0xe1;
const HID_MOD_LALT: u8 = 0xe2;
const HID_MOD_LGUI: u8 = 0xe3;
const HID_MOD_RGUI: u8 = 0xe7;

#[rustfmt::skip]
const KEYMAP: [[u8; 15]; ROWS] = [
    [HID_ESC,        HID_1,        HID_2,        HID_3,        HID_4,     HID_5,  HID_6,  HID_7,     HID_8,    HID_9, HID_0, HID_MINUS,     HID_EQUAL,         HID_BACKSPACE,      HID_NO],
    [HID_TAB,        HID_Q,        HID_W,        HID_E,        HID_R,     HID_T,  HID_NO, HID_Y,     HID_U,    HID_I, HID_O, HID_P,         HID_LEFT_BRACKET,  HID_RIGHT_BRACKET, HID_BACKSLASH],
    [HID_CAPS_LOCK,  HID_A,        HID_S,        HID_D,        HID_F,     HID_G,  HID_NO, HID_H,     HID_J,    HID_K, HID_L, HID_SEMICOLON, HID_QUOTE,         HID_NO,            HID_ENTER],
    [HID_MOD_LSHIFT, HID_Z,        HID_X,        HID_C,        HID_V,     HID_B,  HID_NO, HID_B,     HID_N,    HID_M, HID_COMMA, HID_DOT,   HID_SLASH,         HID_UP,            HID_NO],
    [HID_MOD_LCTRL,  HID_MOD_LGUI, HID_MOD_LALT, HID_NO,       HID_SPACE, HID_NO, HID_NO, HID_SPACE, HID_NO,   HID_NO, HID_NO,    HID_NO,    HID_LEFT,          HID_DOWN,          HID_RIGHT],
];

fn hid_for_pos(row: usize, col: usize) -> Option<HidUsage> {
    let key = KEYMAP[row][col];
    if key == HID_NO {
        None
    } else if (HID_MOD_LCTRL..=HID_MOD_RGUI).contains(&key) {
        Some(HidUsage {
            modifier: 1 << (key - HID_MOD_LCTRL),
            key: 0,
        })
    } else {
        Some(HidUsage { modifier: 0, key })
    }
}

fn notify_hid(sdc: &SoftdeviceController<'_>, report: &[u8; 8]) {
    if !HID_NOTIFY_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let handle = CONN_HANDLE.load(Ordering::Relaxed);
    if handle == CONN_NONE {
        return;
    }

    let mut pdu = [0u8; 11];
    pdu[0] = 0x1b;
    pdu[1..3].copy_from_slice(&HID_REPORT_HANDLE.to_le_bytes());
    pdu[3..11].copy_from_slice(report);
    send_l2cap(sdc, handle, 0x0004, &pdu);
}

async fn handle_hci_event(sdc: &SoftdeviceController<'_>, buf: &[u8]) -> bool {
    if buf.len() < 2 {
        return false;
    }
    if buf[0] == 0x05 && buf.len() >= 6 {
        CONN_HANDLE.store(CONN_NONE, Ordering::Relaxed);
        HID_NOTIFY_ENABLED.store(false, Ordering::Relaxed);
        SMP_STATE.lock(|s| s.borrow_mut().reset_pairing());
        // Clear right-half keys so stale state cannot survive BLE disconnect.
        clear_right_keys();
        defmt::info!("BLE disconnected, cleared right-half keys");
        return true;
    }
    if buf[0] == 0x08 && buf.len() >= 6 {
        let status = buf[2];
        let handle = u16::from_le_bytes([buf[3], buf[4]]) & 0x0fff;
        let enabled = buf[5];
        defmt::info!(
            "BLE encryption change handle={} status={} enabled={}",
            handle,
            status,
            enabled
        );
        if status == 0 && enabled != 0 {
            maybe_send_bond_keys(sdc, handle);
        }
        return false;
    }
    if buf[0] != 0x3e || buf.len() < 4 {
        return false;
    }
    let len = buf[1] as usize;
    if buf.len() < len + 2 || len < 4 {
        return false;
    }
    let data = &buf[2..2 + len];
    defmt::debug!("BLE LE event subevent={}", data[0]);
    match data[0] {
        1 if data.len() >= 12 && data[1] == 0 => {
            let handle = u16::from_le_bytes([data[2], data[3]]) & 0x0fff;
            let peer_addr_type = data[5];
            let mut peer_addr = [0u8; 6];
            peer_addr.copy_from_slice(&data[6..12]);
            on_ble_connected(sdc, handle, peer_addr_type, peer_addr);
        }
        5 if data.len() >= 13 => {
            let handle = u16::from_le_bytes([data[1], data[2]]) & 0x0fff;
            let mut rand = [0u8; 8];
            rand.copy_from_slice(&data[3..11]);
            let ediv = u16::from_le_bytes([data[11], data[12]]);
            reply_ltk_request(sdc, handle, rand, ediv).await;
        }
        10 if data.len() >= 12 && data[1] == 0 => {
            let handle = u16::from_le_bytes([data[2], data[3]]) & 0x0fff;
            let peer_addr_type = data[5];
            let mut peer_addr = [0u8; 6];
            peer_addr.copy_from_slice(&data[6..12]);
            on_ble_connected(sdc, handle, peer_addr_type, peer_addr);
        }
        _ => {}
    }
    false
}

fn on_ble_connected(
    sdc: &SoftdeviceController<'_>,
    handle: u16,
    peer_addr_type: u8,
    peer_addr: [u8; 6],
) {
    CONN_HANDLE.store(handle, Ordering::Relaxed);
    let local_addr = local_addr_bytes();
    let ltk = fixed_ltk(&local_addr);
    SMP_STATE.lock(|s| {
        let mut s = s.borrow_mut();
        s.reset_pairing();
        s.local_addr = local_addr;
        s.peer_addr = peer_addr;
        s.peer_addr_type = peer_addr_type & 0x01;
        s.ltk = ltk;
    });
    defmt::info!("BLE connected handle={}", handle);
    send_l2cap(
        sdc,
        handle,
        0x0006,
        &[SMP_SECURITY_REQUEST, SMP_AUTH_BONDING],
    );
}

async fn reply_ltk_request(sdc: &SoftdeviceController<'_>, handle: u16, rand: [u8; 8], ediv: u16) {
    let key = SMP_STATE.lock(|s| {
        let s = s.borrow();
        if ediv == 0 && rand == [0; 8] && !matches!(s.stage, SmpStage::Idle) {
            Some(s.stk)
        } else if ediv == SMP_FIXED_EDIV && rand == SMP_FIXED_RAND {
            Some(s.ltk)
        } else {
            None
        }
    });
    if let Some(key) = key {
        match LeLongTermKeyRequestReply::new(ConnHandle::new(handle), key)
            .exec(sdc)
            .await
        {
            Ok(_) => defmt::info!("BLE LTK request replied ediv={}", ediv),
            Err(e) => defmt::warn!("BLE LTK request reply failed: {:?}", e),
        }
    } else {
        let _ = LeLongTermKeyRequestNegativeReply::new(ConnHandle::new(handle))
            .exec(sdc)
            .await;
        defmt::warn!("BLE LTK request rejected ediv={}", ediv);
    }
}

fn maybe_send_bond_keys(sdc: &SoftdeviceController<'_>, handle: u16) {
    let keys = SMP_STATE.lock(|s| {
        let mut s = s.borrow_mut();
        if matches!(s.stage, SmpStage::WaitEncrypted) {
            s.stage = SmpStage::Idle;
            Some(s.ltk)
        } else {
            None
        }
    });
    let Some(ltk) = keys else {
        return;
    };

    let mut enc_info = [0u8; 17];
    enc_info[0] = SMP_ENCRYPTION_INFORMATION;
    enc_info[1..17].copy_from_slice(&ltk);
    send_l2cap(sdc, handle, 0x0006, &enc_info);

    let mut master_id = [0u8; 11];
    master_id[0] = SMP_MASTER_IDENTIFICATION;
    master_id[1..3].copy_from_slice(&SMP_FIXED_EDIV.to_le_bytes());
    master_id[3..11].copy_from_slice(&SMP_FIXED_RAND);
    send_l2cap(sdc, handle, 0x0006, &master_id);
    defmt::info!("BLE bond LTK distributed");
}

async fn handle_acl(
    sdc: &SoftdeviceController<'_>,
    mpsl: &MultiprotocolServiceLayer<'_>,
    buf: &[u8],
) {
    if buf.len() < 8 {
        return;
    }
    let handle = u16::from_le_bytes([buf[0], buf[1]]) & 0x0fff;
    let acl_len = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    if acl_len < 4 || buf.len() < 4 + acl_len {
        return;
    }
    let l2cap_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    let cid = u16::from_le_bytes([buf[6], buf[7]]);
    if buf.len() < 8 + l2cap_len {
        return;
    }
    let payload = &buf[8..8 + l2cap_len];
    match cid {
        0x0004 => handle_att(sdc, handle, payload),
        0x0005 => handle_l2cap_control(sdc, handle, payload),
        0x0006 => handle_smp(sdc, mpsl, handle, payload).await,
        _ => {}
    }
}

fn handle_att(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }
    match pdu[0] {
        0x02 => send_l2cap(sdc, handle, 0x0004, &[0x03, 23, 0]),
        0x04 => handle_find_information(sdc, handle, pdu),
        0x08 => handle_read_by_type(sdc, handle, pdu),
        0x0a => handle_read(sdc, handle, pdu),
        0x0c => handle_read_blob(sdc, handle, pdu),
        0x10 => handle_read_by_group_type(sdc, handle, pdu),
        0x12 | 0x52 => handle_write(sdc, handle, pdu),
        opcode => send_att_error(sdc, handle, opcode, req_handle(pdu), 0x06),
    }
}

fn handle_read_by_group_type(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let Some((start, end)) = att_range(pdu) else {
        send_att_error(sdc, handle, 0x10, 0, 0x04);
        return;
    };
    if pdu.len() < 7 || pdu[5] != 0x00 || pdu[6] != 0x28 {
        send_att_error(sdc, handle, 0x10, start, 0x0a);
        return;
    }
    if start <= 1 && end >= 1 {
        send_l2cap(sdc, handle, 0x0004, &[0x11, 6, 1, 0, 5, 0, 0x00, 0x18]);
    } else if start <= 6 && end >= 6 {
        send_l2cap(sdc, handle, 0x0004, &[0x11, 6, 6, 0, 18, 0, 0x12, 0x18]);
    } else {
        send_att_error(sdc, handle, 0x10, start, 0x0a);
    }
}

fn handle_find_information(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let Some((start, end)) = att_range(pdu) else {
        send_att_error(sdc, handle, 0x04, 0, 0x04);
        return;
    };
    let attrs: &[(u16, u16)] = &[
        (2, 0x2803),
        (3, 0x2a00),
        (4, 0x2803),
        (5, 0x2a01),
        (7, 0x2803),
        (8, 0x2a4e),
        (9, 0x2803),
        (10, 0x2a4b),
        (11, 0x2803),
        (12, 0x2a4d),
        (13, 0x2908),
        (14, 0x2902),
        (15, 0x2803),
        (16, 0x2a4a),
        (17, 0x2803),
        (18, 0x2a4c),
    ];
    let mut resp = [0u8; 22];
    resp[0] = 0x05;
    resp[1] = 0x01;
    let mut n = 2;
    for (h, uuid) in attrs {
        if *h >= start && *h <= end && n + 4 <= resp.len() {
            resp[n..n + 2].copy_from_slice(&h.to_le_bytes());
            resp[n + 2..n + 4].copy_from_slice(&uuid.to_le_bytes());
            n += 4;
        }
    }
    if n == 2 {
        send_att_error(sdc, handle, 0x04, start, 0x0a);
    } else {
        send_l2cap(sdc, handle, 0x0004, &resp[..n]);
    }
}

fn handle_read_by_type(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let Some((start, end)) = att_range(pdu) else {
        send_att_error(sdc, handle, 0x08, 0, 0x04);
        return;
    };
    if pdu.len() < 7 {
        send_att_error(sdc, handle, 0x08, start, 0x04);
        return;
    }
    let uuid = u16::from_le_bytes([pdu[5], pdu[6]]);
    if uuid == 0x2803 {
        let chars: &[(u16, u8, u16, u16)] = &[
            (2, 0x02, 3, 0x2a00),
            (4, 0x02, 5, 0x2a01),
            (7, 0x06, 8, 0x2a4e),
            (9, 0x02, 10, 0x2a4b),
            (11, 0x12, 12, 0x2a4d),
            (15, 0x02, 16, 0x2a4a),
            (17, 0x04, 18, 0x2a4c),
        ];
        let mut resp = [0u8; 23];
        resp[0] = 0x09;
        resp[1] = 7;
        let mut n = 2;
        for (decl, props, value, cuuid) in chars {
            if *decl >= start && *decl <= end && n + 7 <= resp.len() {
                resp[n..n + 2].copy_from_slice(&decl.to_le_bytes());
                resp[n + 2] = *props;
                resp[n + 3..n + 5].copy_from_slice(&value.to_le_bytes());
                resp[n + 5..n + 7].copy_from_slice(&cuuid.to_le_bytes());
                n += 7;
            }
        }
        if n > 2 {
            send_l2cap(sdc, handle, 0x0004, &resp[..n]);
        } else {
            send_att_error(sdc, handle, 0x08, start, 0x0a);
        }
    } else if uuid == 0x2a00 && start <= 3 && end >= 3 {
        send_l2cap(sdc, handle, 0x0004, b"\x09\x0f\x03\x00Elytra ESB");
    } else {
        send_att_error(sdc, handle, 0x08, start, 0x0a);
    }
}

fn handle_read(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let attr = req_handle(pdu);
    match attr {
        3 => send_att_read(sdc, handle, b"Elytra ESB"),
        5 => send_att_read(sdc, handle, &[0xc1, 0x03]),
        8 => send_att_read(sdc, handle, &[1]),
        10 => send_att_read(sdc, handle, HID_REPORT_MAP),
        12 => send_att_read(sdc, handle, &[0, 0, 0, 0, 0, 0, 0, 0]),
        13 => send_att_read(sdc, handle, &[1, 1]),
        14 => {
            let v = if HID_NOTIFY_ENABLED.load(Ordering::Relaxed) {
                [1, 0]
            } else {
                [0, 0]
            };
            send_att_read(sdc, handle, &v);
        }
        16 => send_att_read(sdc, handle, &[0x11, 0x01, 0x00, 0x03]),
        _ => send_att_error(sdc, handle, 0x0a, attr, 0x0a),
    }
}

fn handle_read_blob(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.len() < 5 {
        send_att_error(sdc, handle, 0x0c, 0, 0x04);
        return;
    }

    let attr = u16::from_le_bytes([pdu[1], pdu[2]]);
    let offset = u16::from_le_bytes([pdu[3], pdu[4]]) as usize;
    match attr {
        10 => send_att_blob(sdc, handle, attr, HID_REPORT_MAP, offset),
        _ => send_att_error(sdc, handle, 0x0c, attr, 0x0a),
    }
}

fn send_att_read(sdc: &SoftdeviceController<'_>, handle: u16, value: &[u8]) {
    let mut resp = [0u8; 1 + ATT_VALUE_CHUNK];
    let n = value.len().min(ATT_VALUE_CHUNK);
    resp[0] = 0x0b;
    resp[1..1 + n].copy_from_slice(&value[..n]);
    send_l2cap(sdc, handle, 0x0004, &resp[..1 + n]);
}

fn send_att_blob(
    sdc: &SoftdeviceController<'_>,
    handle: u16,
    attr: u16,
    value: &[u8],
    offset: usize,
) {
    if offset > value.len() {
        send_att_error(sdc, handle, 0x0c, attr, 0x07);
        return;
    }

    let mut resp = [0u8; 1 + ATT_VALUE_CHUNK];
    let n = (value.len() - offset).min(ATT_VALUE_CHUNK);
    resp[0] = 0x0d;
    resp[1..1 + n].copy_from_slice(&value[offset..offset + n]);
    send_l2cap(sdc, handle, 0x0004, &resp[..1 + n]);
}

fn handle_write(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.len() < 3 {
        return;
    }
    let attr = u16::from_le_bytes([pdu[1], pdu[2]]);
    if attr == HID_REPORT_CCCD_HANDLE && pdu.len() >= 5 {
        let enabled = pdu[3] & 0x01 != 0;
        HID_NOTIFY_ENABLED.store(enabled, Ordering::Relaxed);
        defmt::info!("HID notify enabled={}", enabled as u8);
    }
    if pdu[0] == 0x12 {
        send_l2cap(sdc, handle, 0x0004, &[0x13]);
    }
}

fn handle_l2cap_control(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.len() < 4 {
        return;
    }
    let code = pdu[0];
    let ident = pdu[1];
    match code {
        0x12 => send_l2cap(sdc, handle, 0x0005, &[0x13, ident, 2, 0, 0, 0]),
        _ => send_l2cap(sdc, handle, 0x0005, &[0x01, ident, 2, 0, code, 0]),
    }
}

async fn handle_smp(
    sdc: &SoftdeviceController<'_>,
    mpsl: &MultiprotocolServiceLayer<'_>,
    handle: u16,
    pdu: &[u8],
) {
    if pdu.is_empty() {
        return;
    }
    match pdu[0] {
        SMP_PAIRING_REQUEST if pdu.len() >= 7 => {
            let mut preq = [0u8; 7];
            preq.copy_from_slice(&pdu[..7]);
            let mut pres = [
                SMP_PAIRING_RESPONSE,
                0x03,
                0x00,
                SMP_AUTH_BONDING,
                16,
                0x00,
                SMP_KEYDIST_ENCKEY,
            ];
            pres[5] = 0x00;
            pres[6] = pdu[6] & SMP_KEYDIST_ENCKEY;
            if pres[6] == 0 {
                pres[6] = SMP_KEYDIST_ENCKEY;
            }

            let srand = match smp_random_128(sdc).await {
                Some(v) => v,
                None => {
                    send_l2cap(sdc, handle, 0x0006, &[SMP_PAIRING_FAILED, 0x08]);
                    return;
                }
            };
            SMP_STATE.lock(|s| {
                let mut s = s.borrow_mut();
                s.stage = SmpStage::WaitConfirm;
                s.preq = preq;
                s.pres = pres;
                s.srand = srand;
                s.stk = [0; 16];
            });
            send_l2cap(sdc, handle, 0x0006, &pres);
            defmt::info!("BLE SMP pairing response sent");
        }
        SMP_PAIRING_CONFIRM if pdu.len() >= 17 => {
            let mut mconfirm = [0u8; 16];
            mconfirm.copy_from_slice(&pdu[1..17]);
            let sconfirm = SMP_STATE.lock(|s| {
                let mut s = s.borrow_mut();
                if !matches!(s.stage, SmpStage::WaitConfirm) {
                    return None;
                }
                s.mconfirm = mconfirm;
                s.stage = SmpStage::WaitRandom;
                Some(pairing_confirm(
                    mpsl,
                    s.srand,
                    s.preq,
                    s.pres,
                    s.peer_addr_type,
                    s.peer_addr,
                    0,
                    s.local_addr,
                ))
            });
            if let Some(sconfirm) = sconfirm {
                let mut resp = [0u8; 17];
                resp[0] = SMP_PAIRING_CONFIRM;
                resp[1..17].copy_from_slice(&sconfirm);
                send_l2cap(sdc, handle, 0x0006, &resp);
                defmt::info!("BLE SMP confirm sent");
            }
        }
        SMP_PAIRING_RANDOM if pdu.len() >= 17 => {
            let mut mrand = [0u8; 16];
            mrand.copy_from_slice(&pdu[1..17]);
            let result = SMP_STATE.lock(|s| {
                let mut s = s.borrow_mut();
                if !matches!(s.stage, SmpStage::WaitRandom) {
                    return None;
                }
                let expected = pairing_confirm(
                    mpsl,
                    mrand,
                    s.preq,
                    s.pres,
                    s.peer_addr_type,
                    s.peer_addr,
                    0,
                    s.local_addr,
                );
                if expected != s.mconfirm {
                    s.reset_pairing();
                    return Some(Err(()));
                }
                s.stk = stk_from_randoms(mpsl, s.srand, mrand);
                s.stage = SmpStage::WaitEncrypted;
                Some(Ok(s.srand))
            });
            match result {
                Some(Ok(srand)) => {
                    let mut resp = [0u8; 17];
                    resp[0] = SMP_PAIRING_RANDOM;
                    resp[1..17].copy_from_slice(&srand);
                    send_l2cap(sdc, handle, 0x0006, &resp);
                    defmt::info!("BLE SMP random sent; waiting for encryption");
                }
                Some(Err(())) => {
                    send_l2cap(sdc, handle, 0x0006, &[SMP_PAIRING_FAILED, 0x04]);
                    defmt::warn!("BLE SMP confirm mismatch");
                }
                None => {}
            }
        }
        opcode => {
            defmt::debug!("BLE SMP opcode={}", opcode);
        }
    }
}

fn req_handle(pdu: &[u8]) -> u16 {
    if pdu.len() >= 3 {
        u16::from_le_bytes([pdu[1], pdu[2]])
    } else {
        0
    }
}

fn att_range(pdu: &[u8]) -> Option<(u16, u16)> {
    if pdu.len() >= 5 {
        Some((
            u16::from_le_bytes([pdu[1], pdu[2]]),
            u16::from_le_bytes([pdu[3], pdu[4]]),
        ))
    } else {
        None
    }
}

fn send_att_error(sdc: &SoftdeviceController<'_>, handle: u16, req: u8, attr: u16, err: u8) {
    let [lo, hi] = attr.to_le_bytes();
    send_l2cap(sdc, handle, 0x0004, &[0x01, req, lo, hi, err]);
}

fn send_l2cap(sdc: &SoftdeviceController<'_>, handle: u16, cid: u16, payload: &[u8]) {
    if payload.len() > 64 {
        return;
    }
    let len = payload.len();
    let mut packet = [0u8; 72];
    packet[0..2].copy_from_slice(&(handle & 0x0fff).to_le_bytes());
    packet[2..4].copy_from_slice(&((len + 4) as u16).to_le_bytes());
    packet[4..6].copy_from_slice(&(len as u16).to_le_bytes());
    packet[6..8].copy_from_slice(&cid.to_le_bytes());
    packet[8..8 + len].copy_from_slice(payload);
    let _ = sdc.hci_data_put(&packet[..8 + len]);
}

async fn smp_random_128(sdc: &SoftdeviceController<'_>) -> Option<[u8; 16]> {
    let lo = LeRand::new().exec(sdc).await.ok()?;
    let hi = LeRand::new().exec(sdc).await.ok()?;
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo);
    out[8..].copy_from_slice(&hi);
    Some(out)
}

fn pairing_confirm(
    mpsl: &MultiprotocolServiceLayer<'_>,
    random: [u8; 16],
    preq: [u8; 7],
    pres: [u8; 7],
    initiator_addr_type: u8,
    initiator_addr: [u8; 6],
    responder_addr_type: u8,
    responder_addr: [u8; 6],
) -> [u8; 16] {
    let tk = [0u8; 16];
    let mut preq_rev = preq;
    preq_rev.reverse();
    let mut pres_rev = pres;
    pres_rev.reverse();
    let mut ia = initiator_addr;
    ia.reverse();
    let mut ra = responder_addr;
    ra.reverse();

    let mut p1 = [0u8; 16];
    p1[0..7].copy_from_slice(&pres_rev);
    p1[7..14].copy_from_slice(&preq_rev);
    p1[14] = responder_addr_type & 0x01;
    p1[15] = initiator_addr_type & 0x01;

    let mut p2 = [0u8; 16];
    p2[4..10].copy_from_slice(&ia);
    p2[10..16].copy_from_slice(&ra);

    let mut r = random;
    r.reverse();
    for i in 0..16 {
        r[i] ^= p1[i];
    }
    let mut block = [0u8; 16];
    let _ = mpsl.ecb_block_encrypt(&tk, &r, &mut block);
    for i in 0..16 {
        block[i] ^= p2[i];
    }
    let mut out = [0u8; 16];
    let _ = mpsl.ecb_block_encrypt(&tk, &block, &mut out);
    out.reverse();
    out
}

fn stk_from_randoms(
    mpsl: &MultiprotocolServiceLayer<'_>,
    responder_random: [u8; 16],
    initiator_random: [u8; 16],
) -> [u8; 16] {
    let tk = [0u8; 16];
    let mut rr_be = responder_random;
    rr_be.reverse();
    let mut ir_be = initiator_random;
    ir_be.reverse();
    let mut clear = [0u8; 16];
    clear[0..8].copy_from_slice(&rr_be[8..16]);
    clear[8..16].copy_from_slice(&ir_be[8..16]);
    let mut out = [0u8; 16];
    let _ = mpsl.ecb_block_encrypt(&tk, &clear, &mut out);
    out.reverse();
    out
}

fn local_addr_bytes() -> [u8; 6] {
    let ficr = embassy_nrf::pac::FICR;
    let addr = (u64::from(ficr.deviceid(1).read()) << 32) | u64::from(ficr.deviceid(0).read());
    let bytes = (addr | 0x0000_c000_0000_0000).to_le_bytes();
    let mut out = [0u8; 6];
    out.copy_from_slice(&bytes[..6]);
    out
}

fn fixed_ltk(local_addr: &[u8; 6]) -> [u8; 16] {
    let mut ltk = [0u8; 16];
    ltk[0..8].copy_from_slice(b"ElytraES");
    ltk[8..14].copy_from_slice(local_addr);
    ltk[14..16].copy_from_slice(&SMP_FIXED_EDIV.to_le_bytes());
    ltk
}

fn bd_addr() -> BdAddr {
    let ficr = embassy_nrf::pac::FICR;
    let addr = (u64::from(ficr.deviceid(1).read()) << 32) | u64::from(ficr.deviceid(0).read());
    BdAddr::new(
        ((addr | 0x0000_c000_0000_0000).to_le_bytes()[..6])
            .try_into()
            .unwrap(),
    )
}

async fn start_advertising(sdc: &SoftdeviceController<'_>) {
    SetEventMask::new(
        EventMask::new()
            .enable_encryption_change_v1(true)
            .enable_le_meta(true)
            .enable_disconnection_complete(true),
    )
    .exec(sdc)
    .await
    .unwrap();
    LeSetEventMask::new(
        LeEventMask::new()
            .enable_le_conn_complete(true)
            .enable_le_long_term_key_request(true)
            .enable_le_enhanced_conn_complete_v1(true),
    )
    .exec(sdc)
    .await
    .unwrap();
    ZephyrWriteBdAddr::new(bd_addr()).exec(sdc).await.unwrap();
    LeSetAdvParams::new(
        bt_hci::param::Duration::from_millis(30),
        bt_hci::param::Duration::from_millis(50),
        AdvKind::AdvInd,
        bt_hci::param::AddrKind::PUBLIC,
        bt_hci::param::AddrKind::PUBLIC,
        BdAddr::default(),
        AdvChannelMap::ALL,
        AdvFilterPolicy::default(),
    )
    .exec(sdc)
    .await
    .unwrap();
    let adv_data = &[
        0x02, 0x01, 0x06, 0x03, 0x03, 0x12, 0x18, 0x03, 0x19, 0xc1, 0x03, 0x0b, 0x09, b'E', b'l',
        b'y', b't', b'r', b'a', b' ', b'E', b'S', b'B',
    ];
    let mut data = [0u8; 31];
    data[..adv_data.len()].copy_from_slice(adv_data);
    LeSetAdvData::new(adv_data.len() as u8, data)
        .exec(sdc)
        .await
        .unwrap();
    LeSetAdvEnable::new(true).exec(sdc).await.unwrap();
}

const HID_REPORT_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

#[embassy_executor::main]
async fn main(spawner: Spawner) {
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
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(
        MultiprotocolServiceLayer::with_timeslots::<typelevel::EGU0_SWI0, _, 1>(
            mpsl_p,
            Irqs,
            lfclk_cfg,
            SESSION_MEM.init(SessionMem::new()),
        )
        .unwrap(),
    );
    spawner.spawn(mpsl_task(mpsl).unwrap());
    spawner.spawn(hfclk_task(mpsl).unwrap());

    // Radio notification: fires at the end of each BLE radio event (INACTIVE).
    // Configured after MPSL is enabled and before the SDC protocol stack starts,
    // else it returns EINPROGRESS. prx_task waits on this signal to open a
    // single ESB RX window in each BLE gap, replacing continuous RX (~4-5mA →
    // ~200µA idle).
    {
        let r = unsafe {
            raw::mpsl_radio_notification_cfg_set(
                raw::MPSL_RADIO_NOTIFICATION_TYPE_INT_ON_INACTIVE as u8,
                raw::MPSL_RADIO_NOTIFICATION_DISTANCE_MIN_US as u16,
                Some(radio_notification_cb),
            )
        };
        defmt::info!("radio notification cfg ret={}", r);
    }

    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    static RNG: StaticCell<Rng> = StaticCell::new();
    let rng = RNG.init(rng::Rng::new_blocking(p.RNG));
    static SDC_MEM: StaticCell<sdc::Mem<8192>> = StaticCell::new();
    static SDC: StaticCell<SoftdeviceController> = StaticCell::new();
    let sdc = SDC.init(
        sdc::Builder::new()
            .unwrap()
            .support_adv()
            .support_peripheral()
            .peripheral_count(1)
            .unwrap()
            .buffer_cfg(64, 64, 4, 4)
            .unwrap()
            .build(sdc_p, rng, mpsl, SDC_MEM.init(sdc::Mem::new()))
            .unwrap(),
    );
    start_advertising(sdc).await;
    spawner.spawn(sdc_task(sdc, mpsl).unwrap());
    spawner.spawn(hid_task(sdc).unwrap());
    spawner.spawn(prx_task(mpsl).unwrap());

    let cols = [
        Output::new(p.P0_30, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_14, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_22, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_05, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_26, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_04, Level::Low, OutputDrive::Standard),
        Output::new(p.P0_27, Level::Low, OutputDrive::Standard),
    ];
    let rows = [
        Input::new(p.P0_00, Pull::Down),
        Input::new(p.P0_01, Pull::Down),
        Input::new(p.P0_03, Pull::Down),
        Input::new(p.P0_25, Pull::Down),
        Input::new(p.P1_03, Pull::Down),
    ];
    spawner.spawn(matrix_scan_task(cols, rows).unwrap());

    defmt::info!("Elytra left central BLE HID + ESB PRX started");
}
