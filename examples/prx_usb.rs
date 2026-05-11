#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::UsbDevice;

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{EsbPrx, DEFAULT_POOL_N, DEFAULT_POOL_SIZE};
use embassy_nrf_esb::pac;
use embassy_nrf_esb::payload::PacketPool;

// No defmt_rtt — no debugger attached. Panic handler loops forever.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        cortex_m::asm::wfe();
    }
}

mod interrupt {
    pub use embassy_nrf_esb::pac::Interrupt::*;
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

    // USB requires external HFCLK
    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    // USB CDC setup
    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("ESB Test");
    config.product = Some("PRX USB CDC");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static MSOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static CDC_STATE: static_cell::StaticCell<State<'static>> = static_cell::StaticCell::new();

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

    // ESB PRX setup
    let esb_config = EsbConfig::default();
    let addresses = EsbAddresses::default();

    let prx = {
        static ESB: static_cell::StaticCell<EsbPrx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPrx::new(
            p.TIMER1, p.RADIO, &POOL, &esb_config, &addresses,
        ))
    };
    unsafe { PRX_REF = Some(prx) };

    class.wait_connection().await;
    let _ = class.write_packet(b"[ESB PRX] Init OK\r\n").await;

    // Step 1: Dump RADIO registers via USB CDC
    dump_radio_regs(&mut class).await;

    let _ = class.write_packet(b"\r\n[ESB PRX] Listening...\r\n").await;
    prx.start_listening().expect("start_listening failed");

    let mut rx_count: u32 = 0;
    let mut buf = [0u8; 128];
    loop {
        let pkt = prx.receive().await;
        rx_count += 1;

        let len = format_packet(&mut buf, rx_count, pkt.pipe(), pkt.len(), pkt.payload());
        let _ = class.write_packet(&buf[..len]).await;
    }
}

async fn dump_radio_regs(class: &mut CdcAcmClass<'static, MyUsbDriver>) {
    let r = pac::RADIO;

    let regs: [(&str, u32); 12] = [
        ("FREQUENCY", r.frequency().read().frequency() as u32),
        ("MODE", r.mode().read().mode() as u32),
        ("PCNF0", r.pcnf0().read().0),
        ("PCNF1", r.pcnf1().read().0),
        ("CRCCNF", r.crccnf().read().0),
        ("CRCPOLY", r.crcpoly().read().0),
        ("CRCINIT", r.crcinit().read().0),
        ("BASE0", r.base0().read()),
        ("BASE1", r.base1().read()),
        ("PREFIX0", r.prefix0().read().0),
        ("PREFIX1", r.prefix1().read().0),
        ("TXPOWER", r.txpower().read().0),
    ];

    let _ = class.write_packet(b"--- RADIO Registers ---\r\n").await;
    let mut buf = [0u8; 64];
    for (name, val) in regs {
        let len = {
            let mut w = WriteBuf::new(&mut buf);
            let _ = write!(w, "  {}: 0x{:08X}\r\n", name, val);
            w.pos
        };
        let _ = class.write_packet(&buf[..len]).await;
    }
    let _ = class.write_packet(b"--- End ---\r\n").await;
}

fn format_packet(buf: &mut [u8], count: u32, pipe: u8, len: usize, data: &[u8]) -> usize {
    let mut w = WriteBuf::new(buf);
    let _ = write!(w, "[RX] #{}: pipe={} len={} data=", count, pipe, len);
    for &b in data.iter().take(32) {
        let _ = write!(w, "{:02x}", b);
    }
    let _ = write!(w, "\r\n");
    w.pos
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
