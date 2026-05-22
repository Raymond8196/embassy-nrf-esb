#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_time::Timer;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPtx};
use embassy_nrf_esb::payload::PacketPool;

use {defmt_rtt as _, panic_probe as _};

mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PTX_REF: Option<&'static EsbPtx<TIMER1>> = None;

type MyUsbDriver = UsbDriver<'static, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0001);
    usb_config.manufacturer = Some("ESB Test");
    usb_config.product = Some("PTX multipipe ACK");
    usb_config.serial_number = Some("PTXMP001");
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

    let config = EsbConfig::default();
    let addresses = EsbAddresses::default();

    let ptx = {
        static ESB: static_cell::StaticCell<EsbPtx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPtx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses, 0).unwrap())
    };
    unsafe { PTX_REF = Some(ptx) };

    class.wait_connection().await;
    let _ = class.write_packet(b"[PTX MP] Ready\r\n").await;

    let mut tx = [0u32; 2];
    let mut ack = [0u32; 2];
    let mut tx_full = 0u32;
    let mut max_attempts = 0u32;
    let mut invalid_ack = 0u32;
    let mut counter = 0u32;
    let mut buf = [0u8; 64];

    loop {
        let pipe = (counter & 1) as u8;
        let mut payload = [0u8; 5];
        payload[..4].copy_from_slice(&counter.to_le_bytes());
        payload[4] = pipe;

        match ptx.send_to(pipe, &payload).await {
            Ok(()) => tx[pipe as usize] += 1,
            Err(_) => tx_full += 1,
        }

        if ptx.max_attempts_reached() {
            max_attempts += 1;
        }

        while let Some(pkt) = ptx.try_receive() {
            let data = pkt.payload();
            if data.len() >= 5 {
                let ack_pipe = data[0];
                if ack_pipe < 2 {
                    ack[ack_pipe as usize] += 1;
                } else {
                    invalid_ack += 1;
                }
            } else {
                invalid_ack += 1;
            }
        }

        if counter % 200 == 0 && counter > 0 {
            let len = {
                let mut w = WriteBuf::new(&mut buf);
                let _ = write!(
                    w,
                    "[T] q0={} a0={} q1={} a1={} f={} m={} i={}\r\n",
                    tx[0], ack[0], tx[1], ack[1], tx_full, max_attempts, invalid_ack
                );
                w.pos
            };
            let _ = class.write_packet(&buf[..len]).await;
        }

        counter = counter.wrapping_add(1);
        Timer::after_millis(10).await;
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
