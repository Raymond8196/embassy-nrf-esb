#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPrx};
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
static mut PRX_REF: Option<&'static EsbPrx<TIMER1>> = None;

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
    usb_config.product = Some("PRX Idle");
    usb_config.serial_number = Some("PRXIDLE");
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

    let prx = {
        static ESB: static_cell::StaticCell<EsbPrx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPrx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses).unwrap())
    };
    unsafe { PRX_REF = Some(prx) };

    class.wait_connection().await;
    let _ = class.write_packet(b"[PRX Idle] Ready\r\n").await;

    let mut rx = 0u32;
    let mut cycle = 0u32;
    let mut cycle_rx: u32;
    let mut empty_cycles = 0u32;
    let mut malformed = 0u32;
    let mut buf = [0u8; 112];

    loop {
        cycle += 1;
        cycle_rx = 0;
        prx.start_listening().expect("start_listening failed");

        let until = Instant::now() + Duration::from_millis(950);
        while Instant::now() < until {
            match embassy_time::with_timeout(Duration::from_millis(50), prx.receive()).await {
                Ok(pkt) => {
                    rx += 1;
                    cycle_rx += 1;
                    if pkt.payload().len() < 4 {
                        malformed += 1;
                    }
                }
                Err(_) => {}
            }
        }

        prx.stop();
        if cycle_rx == 0 {
            empty_cycles += 1;
        }

        if cycle % 10 == 0 {
            let len = {
                let mut w = WriteBuf::new(&mut buf);
                let _ = write!(
                    w,
                    "[I] c={} rx={} last={} empty={} mf={}\r\n",
                    cycle, rx, cycle_rx, empty_cycles, malformed
                );
                w.pos
            };
            let _ = class.write_packet(&buf[..len]).await;
        }

        Timer::after_millis(50).await;
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
