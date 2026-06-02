//! PRX split central prototype.
//!
//! Receives ESB packets from the peripheral, decodes the transport frame,
//! deserializes the RMK SplitMessage::Key, and types the corresponding
//! character via USB HID keyboard.
//!
//! Key mapping (row=0, col=0..4) -> "hello"
//!
//! Flash: make flash-prx_split_central PORT=/dev/ttyACMx

#![no_std]
#![no_main]

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
use embassy_nrf_esb::transport::{self, SequenceTracker, StaticBindingTable};

use serde::{Deserialize, Serialize};

use panic_halt as _;
mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct KeyPos {
    pub row: u8,
    pub col: u8,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum KeyboardEventPos {
    Key(KeyPos),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct KeyboardEvent {
    pub pressed: bool,
    pub pos: KeyboardEventPos,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum SplitMessage {
    Key(KeyboardEvent),
}

const SPLIT_MESSAGE_MAX_SIZE: usize = 32;

const HID_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08,
    0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

const KEY_NONE: u8 = 0x00;
const HID_KEY_H: u8 = 0x0B;
const HID_KEY_E: u8 = 0x08;
const HID_KEY_L: u8 = 0x0F;
const HID_KEY_O: u8 = 0x12;

const KEY_MAP: [u8; 5] = [HID_KEY_H, HID_KEY_E, HID_KEY_L, HID_KEY_L, HID_KEY_O];

const REPORT_SIZE: usize = 8;

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PRX_REF: Option<&'static EsbPrx<TIMER1>> = None;

type MyUsbDriver = UsbDriver<'static, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await;
}

fn make_report(key: u8) -> [u8; REPORT_SIZE] {
    [
        0u8, 0, key, KEY_NONE, KEY_NONE, KEY_NONE, KEY_NONE, KEY_NONE,
    ]
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0002);
    usb_config.manufacturer = Some("ESB Split");
    usb_config.product = Some("PRX Central");
    usb_config.serial_number = Some("PRX-SPLIT-1");
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
        poll_ms: 10,
        max_packet_size: 64,
        hid_subclass: HidSubclass::Boot,
        hid_boot_protocol: HidBootProtocol::Keyboard,
    };

    let mut hid_writer: HidWriter<'static, MyUsbDriver, REPORT_SIZE> =
        HidWriter::new(&mut builder, HID_STATE.init(HidState::new()), hid_config);

    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());

    let required_payload = transport::required_esb_payload_len(SPLIT_MESSAGE_MAX_SIZE);
    let esb_config = EsbConfig::default().with_payload_length(required_payload as u8);
    let addresses = EsbAddresses::default();

    let prx = {
        static ESB: static_cell::StaticCell<EsbPrx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPrx::new(p.TIMER1, p.RADIO, &POOL, &esb_config, &addresses).unwrap())
    };
    unsafe { PRX_REF = Some(prx) };

    hid_writer.ready().await;

    let empty_report = make_report(KEY_NONE);
    let _ = hid_writer.write(&empty_report).await;

    prx.start_listening().expect("start_listening failed");

    let mut bindings = StaticBindingTable::<8>::new();
    bindings.bind(0, 0).unwrap();
    let mut seq_tracker = SequenceTracker::<8>::new();

    let mut rx_count: u32 = 0;
    let mut key_count: u32 = 0;

    loop {
        let pkt = prx.receive().await;
        rx_count += 1;

        let data = pkt.payload();
        let pipe = pkt.pipe();

        let accept_result = transport::accept_bound_frame(&bindings, &mut seq_tracker, pipe, data);

        match accept_result {
            Ok(Some((_header, payload))) => match postcard::from_bytes::<SplitMessage>(payload) {
                Ok(SplitMessage::Key(ev)) => match ev.pos {
                    KeyboardEventPos::Key(kp) => {
                        let hid_key = if (kp.row as usize) < 1 && (kp.col as usize) < KEY_MAP.len()
                        {
                            KEY_MAP[kp.col as usize]
                        } else {
                            continue;
                        };

                        if ev.pressed {
                            let report = make_report(hid_key);
                            let _ = hid_writer.write(&report).await;
                            key_count += 1;
                        } else {
                            let report = make_report(KEY_NONE);
                            let _ = hid_writer.write(&report).await;
                        }

                        let ack = key_count.to_le_bytes();
                        let _ = prx.send_ack_payload(pipe, &ack).await;
                    }
                },
                Err(_) => {}
            },
            Ok(None) => {
                let ack = key_count.to_le_bytes();
                let _ = prx.send_ack_payload(pipe, &ack).await;
            }
            Err(_) => {}
        }
    }
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
