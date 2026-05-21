//! Debug PTX with USB CDC to verify it's running.

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
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPtx};
use embassy_nrf_esb::payload::PacketPool;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
});

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>) {
    device.run().await
}

mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PTX_REF: Option<&'static EsbPtx<peripherals::TIMER1>> = None;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    // USB CDC setup
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);
    let mut config = embassy_usb::Config::new(0x1209, 0x0002);
    config.product = Some("PTX debug");
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
    let _ = class.write_packet(b"PTX: USB ready\r\n").await;

    // HFCLK
    let clock = pac::CLOCK;
    clock.tasks_hfclkstart().write_value(1);
    let _ = class.write_packet(b"PTX: HFCLK requested\r\n").await;
    while clock.events_hfclkstarted().read() != 1 {}
    let _ = class.write_packet(b"PTX: HFCLK started\r\n").await;

    // ESB PTX
    let config = EsbConfig::default();
    let addresses = EsbAddresses::default();
    let ptx = {
        static ESB: StaticCell<EsbPtx<peripherals::TIMER1>> = StaticCell::new();
        &*ESB.init(EsbPtx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses, 0).unwrap())
    };
    unsafe { PTX_REF = Some(ptx) };

    // Dump RADIO config
    {
        let r = pac::RADIO;
        let freq = r.frequency().read().0;
        let b0 = r.base0().read();
        let p0 = r.prefix0().read().0;
        let txadd = r.txaddress().read().0;
        let mut buf = [0u8; 128];
        let mut w = FmtBuf::new(&mut buf);
        let _ = write!(
            w,
            "FREQ={} BASE0={:08x} PREFIX0={:08x} TXADDR={}\r\n",
            freq, b0, p0, txadd
        );
        let _ = class.write_packet(w.bytes()).await;
    }

    let mut counter: u32 = 0;
    let mut ok_count: u32 = 0;
    let mut err_count: u32 = 0;
    loop {
        let payload = counter.to_le_bytes();
        match ptx.send(&payload).await {
            Ok(_) => ok_count += 1,
            Err(_) => err_count += 1,
        }
        if counter % 100 == 0 {
            let mut buf = [0u8; 64];
            let mut w = FmtBuf::new(&mut buf);
            let _ = write!(w, "PTX: {} ok={} err={}\r\n", counter, ok_count, err_count);
            let _ = class.write_packet(w.bytes()).await;
        }
        counter = counter.wrapping_add(1);
        embassy_time::Timer::after_millis(10).await;
    }
}

struct FmtBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> FmtBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn bytes(&self) -> &[u8] {
        &self.buf[..self.pos]
    }
}

impl core::fmt::Write for FmtBuf<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = (self.pos + bytes.len()).min(self.buf.len());
        let count = end - self.pos;
        self.buf[self.pos..end].copy_from_slice(&bytes[..count]);
        self.pos = end;
        Ok(())
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
