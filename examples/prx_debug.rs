//! Debug PRX — reports RADIO state via USB CDC.

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use embassy_executor::Spawner;
use embassy_nrf::pac;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPrx};
use embassy_nrf_esb::payload::PacketPool;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

struct WriteBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}
impl<'a> WriteBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn bytes(&self) -> &[u8] {
        &self.buf[..self.pos]
    }
}
impl core::fmt::Write for WriteBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        let end = (self.pos + b.len()).min(self.buf.len());
        self.buf[self.pos..end].copy_from_slice(&b[..end - self.pos]);
        self.pos = end;
        Ok(())
    }
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
});

mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>) {
    device.run().await
}

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PRX_REF: Option<&'static EsbPrx<peripherals::TIMER1>> = None;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);
    let mut config = embassy_usb::Config::new(0x1209, 0x0003);
    config.product = Some("PRX debug");

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static CDC_STATE: StaticCell<State<'static>> = StaticCell::new();

    let mut builder = embassy_usb::Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );
    let mut class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());

    class.wait_connection().await;
    let _ = class.write_packet(b"PRX debug: ready\r\n").await;

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::default();
    let prx = {
        static ESB: StaticCell<EsbPrx<peripherals::TIMER1>> = StaticCell::new();
        &*ESB.init(EsbPrx::new(p.TIMER1, p.RADIO, &POOL, &esb_cfg, &esb_addr).unwrap())
    };
    unsafe { PRX_REF = Some(prx) };

    // Dump RADIO config
    {
        let r = pac::RADIO;
        let freq = r.frequency().read().0;
        let b0 = r.base0().read();
        let b1 = r.base1().read();
        let p0 = r.prefix0().read().0;
        let p1 = r.prefix1().read().0;
        let rxa = r.rxaddresses().read().0;
        let mut buf = [0u8; 128];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "FREQ={} B0={:08x} B1={:08x} P0={:08x} P1={:08x} RXA={}\r\n",
            freq, b0, b1, p0, p1, rxa
        );
        let _ = class.write_packet(w.bytes()).await;
    }

    prx.start_listening().expect("start_listening failed");

    {
        let mut buf = [0u8; 64];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "state={}, RXA={}\r\n",
            pac::RADIO.state().read().0,
            pac::RADIO.rxaddresses().read().0
        );
        let _ = class.write_packet(w.bytes()).await;
    }

    let mut sec = 0u32;
    loop {
        embassy_time::Timer::after_secs(1).await;
        sec += 1;
        let mut buf = [0u8; 64];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "[{}] RADIO={} RXA={}\r\n",
            sec,
            pac::RADIO.state().read().0,
            pac::RADIO.rxaddresses().read().0
        );
        let _ = class.write_packet(w.bytes()).await;
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
