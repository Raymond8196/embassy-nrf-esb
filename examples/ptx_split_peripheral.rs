//! PTX split peripheral prototype.
//!
//! Simulates key events on row 0, cycling through columns 0-5.
//! Each event is serialized as a RMK SplitMessage::Key via postcard,
//! wrapped in an ESB transport frame, and sent to the central.
//!
//! USB CDC provides status output.
//!
//! Flash: make flash-ptx_split_peripheral PORT=/dev/ttyACMx

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_time::{Timer, with_deadline};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};

use embassy_nrf::pac;
use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPtx};
use embassy_nrf_esb::payload::PacketPool;
use embassy_nrf_esb::transport;

use serde::{Deserialize, Serialize};

use {defmt_rtt as _, panic_probe as _};

mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

// ---- RMK-compatible SplitMessage (minimal subset) ----

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

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PTX_REF: Option<&'static EsbPtx<TIMER1>> = None;

type MyUsbDriver = UsbDriver<'static, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await;
}

async fn cdc_log(class: &mut CdcAcmClass<'static, MyUsbDriver>, data: &[u8]) {
    let _ = with_deadline(
        embassy_time::Instant::from_millis(10),
        class.write_packet(data),
    )
    .await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0001);
    usb_config.manufacturer = Some("ESB Split");
    usb_config.product = Some("PTX Peripheral");
    usb_config.serial_number = Some("PTX-SPLIT-1");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static MSOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static CDC_STATE: static_cell::StaticCell<State<'static>> = static_cell::StaticCell::new();

    let mut builder = embassy_usb::Builder::new(
        driver,
        usb_config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let mut class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());

    // ESB setup — payload must fit transport header + max split message
    let required_payload = transport::required_esb_payload_len(SPLIT_MESSAGE_MAX_SIZE);
    let esb_config = EsbConfig::default().with_payload_length(required_payload as u8);
    let addresses = EsbAddresses::default();

    let ptx = {
        static ESB: static_cell::StaticCell<EsbPtx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPtx::new(p.TIMER1, p.RADIO, &POOL, &esb_config, &addresses, 0).unwrap())
    };
    unsafe { PTX_REF = Some(ptx) };

    class.wait_connection().await;
    cdc_log(&mut class, b"[PTX SPLIT] Ready - simulating key events\r\n").await;

    let mut serial_buf = [0u8; SPLIT_MESSAGE_MAX_SIZE];
    let mut frame_buf = [0u8; 64];
    let mut tx_count: u32 = 0;
    let mut sequence: u8 = 0;
    let mut col: u8 = 0;
    const NUM_COLS: u8 = 5;
    let mut pressed = true;
    let mut usb_buf = [0u8; 128];

    loop {
        let msg = SplitMessage::Key(KeyboardEvent {
            pressed,
            pos: KeyboardEventPos::Key(KeyPos { row: 0, col }),
        });

        let serialized = postcard::to_slice(&msg, &mut serial_buf).unwrap();

        let frame_len =
            transport::encode_frame(0, sequence, 0, serialized, &mut frame_buf).unwrap();

        match ptx.send(&frame_buf[..frame_len]).await {
            Ok(()) => {}
            Err(e) => {
                let len = {
                    let mut w = WriteBuf::new(&mut usb_buf);
                    let _ = write!(w, "[ERR] send failed: {:?}\r\n", e);
                    w.pos
                };
                cdc_log(&mut class, &usb_buf[..len]).await;
            }
        }

        tx_count += 1;
        sequence = sequence.wrapping_add(1);

        if tx_count % 50 == 0 {
            let len = {
                let mut w = WriteBuf::new(&mut usb_buf);
                let _ = write!(
                    w,
                    "[PTX] tx={} seq={} col={} pressed={}\r\n",
                    tx_count, sequence, col, pressed
                );
                w.pos
            };
            cdc_log(&mut class, &usb_buf[..len]).await;
        }

        // Check for ACK payload (central → peripheral messages)
        if let Some(ack) = ptx.try_receive() {
            let ack_data = ack.payload();
            if tx_count % 50 == 0 {
                let len = {
                    let mut w = WriteBuf::new(&mut usb_buf);
                    let _ = write!(w, "[ACK] len={}", ack_data.len());
                    for &b in ack_data.iter().take(8) {
                        let _ = write!(w, " {:02x}", b);
                    }
                    let _ = write!(w, "\r\n");
                    w.pos
                };
                cdc_log(&mut class, &usb_buf[..len]).await;
            }
        }

        // Next key event: toggle pressed, advance col after release
        if pressed {
            pressed = false;
        } else {
            pressed = true;
            col = (col + 1) % NUM_COLS;
        }

        Timer::after_millis(100).await;
    }
}

#[cortex_m_rt::interrupt]
fn RADIO() {
    if let Some(ptx) = unsafe { PTX_REF } {
        ptx.on_radio_interrupt();
    }
}

#[cortex_m_rt::interrupt]
fn TIMER1() {
    if let Some(ptx) = unsafe { PTX_REF } {
        ptx.on_timer_interrupt();
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
