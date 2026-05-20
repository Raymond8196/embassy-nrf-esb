#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};

use embassy_nrf::pac;
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
static STATS_CH: Channel<CriticalSectionRawMutex, [u32; 3], 2> = Channel::new();

type MyUsbDriver = UsbDriver<'static, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await;
}

#[embassy_executor::task]
async fn stats_reporter(mut class: CdcAcmClass<'static, MyUsbDriver>) {
    let mut buf = [0u8; 128];
    loop {
        let [rx_count, lost, _] = STATS_CH.receive().await;
        let loss_pct = if rx_count + lost > 0 {
            lost as u32 * 10000 / (rx_count + lost)
        } else {
            0
        };
        let len = {
            let mut w = WriteBuf::new(&mut buf);
            let _ = write!(
                w,
                "[STAT] rx={} lost={} loss={}.{}%\r\n",
                rx_count,
                lost,
                loss_pct / 100,
                loss_pct % 100
            );
            w.pos
        };
        let _ = class.write_packet(&buf[..len]).await;
    }
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
        &*ESB.init(EsbPrx::new(p.TIMER1, p.RADIO, &POOL, &esb_config, &addresses).unwrap())
    };
    unsafe { PRX_REF = Some(prx) };

    class.wait_connection().await;
    let _ = class.write_packet(b"[ESB PRX] Listening...\r\n").await;
    spawner.spawn(stats_reporter(class).unwrap());

    prx.start_listening().expect("start_listening failed");

    let mut rx_count: u32 = 0;
    let mut lost: u32 = 0;
    let mut last_counter: Option<u32> = None;
    let mut report_interval: u32 = 0;
    loop {
        let pkt = prx.receive().await;
        rx_count += 1;
        report_interval += 1;

        let data = pkt.payload();
        let pipe = pkt.pipe();
        if data.len() >= 4 {
            let counter = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if let Some(prev) = last_counter {
                let gap = counter.wrapping_sub(prev);
                if gap > 1 && gap < 0x8000_0000 {
                    lost += gap - 1;
                }
            }
            last_counter = Some(counter);

            // Echo counter back as ACK payload
            let ack_payload = counter.to_le_bytes();
            let _ = prx.send_ack_payload(pipe, &ack_payload).await;
        }

        if report_interval >= 200 {
            report_interval = 0;
            let _ = STATS_CH.try_send([rx_count, lost, 0]);
        }
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
