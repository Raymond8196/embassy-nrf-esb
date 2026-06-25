//! Elytra dongle central: pure 2.4G mode (G2).
//!
//! nRF52840 dongle firmware. Receives ESB packets from both keyboard halves
//! via exclusive-mode multi-pipe PRX, merges key states, and emits USB HID.
//!
//! Topology:
//! - Pipe 0 = left half (device_id 1)
//! - Pipe 1 = right half (device_id 0)
//!
//! No MPSL, no BLE, no SoftDevice — pure exclusive ESB for lowest latency.
//!
//! Build: cargo build --example dongle_central --features nrf52840,defmt,_cs-cortex
//! Flash: make flash-dongle_central PORT=/dev/ttyACMx

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::UsbDevice;
use embassy_usb::class::hid::{
    Config as HidConfig, HidBootProtocol, HidSubclass, HidWriter, State as HidState,
};

use embassy_nrf::pac;
use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPrx};
use embassy_nrf_esb::payload::PacketPool;
use embassy_nrf_esb::transport::{self, SequenceTracker, StaticBindingTable, TransportAck};

use {defmt_rtt as _, panic_halt as _};
mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

// ---- Matrix / keymap constants (mirror left_central.rs) ----

const ROWS: usize = 5;
const LEFT_COLS: usize = 7;
const RIGHT_COLS: usize = 8;
const TOTAL_COLS: usize = LEFT_COLS + RIGHT_COLS;
const TOTAL_KEYS: usize = ROWS * TOTAL_COLS;

const SNAPSHOT_MSG: u8 = 0x53;
const SNAPSHOT_PAYLOAD_LEN: usize = 1 + ROWS;

const LEFT_DEVICE_ID: u8 = 1;
const RIGHT_DEVICE_ID: u8 = 0;

const REPORT_SIZE: usize = 8;

// ---- HID key codes ----

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
const KEYMAP: [[u8; TOTAL_COLS]; ROWS] = [
    [HID_ESC,        HID_1,        HID_2,        HID_3,        HID_4,     HID_5,  HID_6,  HID_7,     HID_8,    HID_9, HID_0, HID_MINUS,     HID_EQUAL,         HID_BACKSPACE,      HID_NO],
    [HID_TAB,        HID_Q,        HID_W,        HID_E,        HID_R,     HID_T,  HID_NO, HID_Y,     HID_U,    HID_I, HID_O, HID_P,         HID_LEFT_BRACKET,  HID_RIGHT_BRACKET, HID_BACKSLASH],
    [HID_CAPS_LOCK,  HID_A,        HID_S,        HID_D,        HID_F,     HID_G,  HID_NO, HID_H,     HID_J,    HID_K, HID_L, HID_SEMICOLON, HID_QUOTE,         HID_NO,            HID_ENTER],
    [HID_MOD_LSHIFT, HID_Z,        HID_X,        HID_C,        HID_V,     HID_B,  HID_NO, HID_B,     HID_N,    HID_M, HID_COMMA, HID_DOT,   HID_SLASH,         HID_UP,            HID_NO],
    [HID_MOD_LCTRL,  HID_MOD_LGUI, HID_MOD_LALT, HID_NO,       HID_SPACE, HID_NO, HID_NO, HID_SPACE, HID_NO,   HID_NO, HID_NO,    HID_NO,    HID_LEFT,          HID_DOWN,          HID_RIGHT],
];

struct HidUsage {
    modifier: u8,
    key: u8,
}

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

const HID_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08,
    0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

// ---- ESB setup ----

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PRX_REF: Option<&'static EsbPrx<TIMER1>> = None;

static RX_COUNT: AtomicU32 = AtomicU32::new(0);
static DUP_COUNT: AtomicU32 = AtomicU32::new(0);
static ERR_COUNT: AtomicU32 = AtomicU32::new(0);
static LEFT_RX: AtomicU32 = AtomicU32::new(0);
static RIGHT_RX: AtomicU32 = AtomicU32::new(0);

type MyUsbDriver = UsbDriver<'static, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    // Start HFCLK (exclusive mode owns the clock directly).
    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    // ---- USB HID setup ----
    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0003);
    usb_config.manufacturer = Some("Elytra");
    usb_config.product = Some("Dongle Central");
    usb_config.serial_number = Some("DONGLE-G2");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static MSOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static HID_STATE: static_cell::StaticCell<HidState<'static>> = static_cell::StaticCell::new();

    let mut builder = embassy_usb::Builder::new(
        driver,
        usb_config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let hid_config = HidConfig {
        report_descriptor: HID_REPORT_DESC,
        request_handler: None,
        poll_ms: 1,
        max_packet_size: 64,
        hid_subclass: HidSubclass::Boot,
        hid_boot_protocol: HidBootProtocol::Keyboard,
    };

    let mut hid_writer: HidWriter<'static, MyUsbDriver, REPORT_SIZE> =
        HidWriter::new(&mut builder, HID_STATE.init(HidState::new()), hid_config);

    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());

    // ---- ESB PRX setup (exclusive mode, multi-pipe) ----
    let esb_config = EsbConfig::default().with_payload_length(16);
    let addresses = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    let prx = {
        static ESB: static_cell::StaticCell<EsbPrx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPrx::new(p.TIMER1, p.RADIO, &POOL, &esb_config, &addresses).unwrap())
    };
    unsafe { PRX_REF = Some(prx) };

    hid_writer.ready().await;
    let _ = hid_writer.write(&[0u8; REPORT_SIZE]).await;

    prx.start_listening().expect("start_listening failed");

    // Transport: pipe 0 = left (device_id 1), pipe 1 = right (device_id 0).
    let mut bindings = StaticBindingTable::<8>::new();
    bindings.bind(0, LEFT_DEVICE_ID).unwrap();
    bindings.bind(1, RIGHT_DEVICE_ID).unwrap();
    let mut seq_tracker = SequenceTracker::<8>::new();

    // Merged key state from both halves.
    let mut active = [false; TOTAL_KEYS];

    defmt::info!("Dongle central started: pipe0=left, pipe1=right");

    loop {
        let pkt = prx.receive().await;
        RX_COUNT.fetch_add(1, Ordering::Relaxed);

        let data = pkt.payload();
        let pipe = pkt.pipe();

        let (header, payload) =
            match transport::accept_bound_frame(&bindings, &mut seq_tracker, pipe, data) {
                Ok(Some((header, payload))) => (header, payload),
                Ok(None) => {
                    DUP_COUNT.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(_) => {
                    ERR_COUNT.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

        match pipe {
            0 => {
                LEFT_RX.fetch_add(1, Ordering::Relaxed);
            }
            1 => {
                RIGHT_RX.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }

        // Apply snapshot from either half.
        if payload.len() == SNAPSHOT_PAYLOAD_LEN && payload[0] == SNAPSHOT_MSG {
            let col_offset = if pipe == 0 { 0 } else { LEFT_COLS };
            let n_cols = if pipe == 0 { LEFT_COLS } else { RIGHT_COLS };

            for row in 0..ROWS {
                let mask = payload[1 + row];
                for col in 0..n_cols {
                    let pos = row * TOTAL_COLS + col_offset + col;
                    if pos < TOTAL_KEYS {
                        active[pos] = mask & (1 << col) != 0;
                    }
                }
            }
        }

        // Build and send HID report.
        let report = build_hid_report(&active);
        let _ = hid_writer.write(&report).await;

        // Send transport ACK.
        let mut ack_buf = [0u8; 4];
        if transport::encode_transport_ack(
            TransportAck::new(header.device_id, header.sequence),
            &mut ack_buf,
        )
        .is_ok()
        {
            let _ = prx.send_ack_payload(pipe, &ack_buf).await;
        }
    }
}

/// Build a 6KRO HID report from the merged active key state.
fn build_hid_report(active: &[bool; TOTAL_KEYS]) -> [u8; REPORT_SIZE] {
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

    let mut report = [0u8; REPORT_SIZE];
    report[0] = modifiers;
    report[2..8].copy_from_slice(&pressed);
    report
}

#[cortex_m_rt::interrupt]
fn RADIO() {
    if let Some(prx) = unsafe { PRX_REF } {
        prx.on_radio_interrupt();
    }
}

#[cortex_m_rt::interrupt]
fn TIMER1() {
    if let Some(prx) = unsafe { PRX_REF } {
        prx.on_timer_interrupt();
    }
}
