//! M10 Step 5: PRX reception inside MPSL timeslots (USB CDC).
//!
//! Each 6 ms timeslot power-cycles the RADIO, inits ESB PRX registers,
//! listens for incoming packets, and sends ACKs. Runs chained slots
//! continuously, reporting stats via USB CDC.
//!
//! Pass criteria (docs/m10-plan.md Step 5):
//!   - 60 seconds: rx_count >= 4000 (67% duty cycle baseline)
//!   - Slot-internal bad_crc < 1%
//!   - 0 OVERSTAYED
//!
//! Test counterpart: ptx_silent on second dongle (10ms interval).

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::UsbDevice;
use nrf_mpsl::{raw, MultiprotocolServiceLayer, Peripherals, SessionMem};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_timeslot::run_prx_slots;

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
        let bytes = s.as_bytes();
        let end = (self.pos + bytes.len()).min(self.buf.len());
        let count = end - self.pos;
        self.buf[self.pos..end].copy_from_slice(&bytes[..count]);
        self.pos = end;
        Ok(())
    }
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    EGU0_SWI0 => nrf_mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_mpsl::ClockInterruptHandler;
    RADIO => nrf_mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_mpsl::HighPrioInterruptHandler;
});

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

#[embassy_executor::task]
async fn usb_task(
    mut device: UsbDevice<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>,
) {
    device.run().await
}

#[embassy_executor::task]
async fn hfclk_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    let _hfclk = mpsl.request_hfclk().await.unwrap();
    core::future::pending().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let lfclk_cfg = raw::mpsl_clock_lfclk_cfg_t {
        source: raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: 16,
        rc_temp_ctiv: 2,
        accuracy_ppm: 500,
        skip_wait_lfclk_started: false,
    };

    let mpsl_p = Peripherals::new(
        p.RTC0,
        p.TIMER0,
        p.TEMP,
        p.PPI_CH19,
        p.PPI_CH30,
        p.PPI_CH31,
    );

    static SESSION_MEM: StaticCell<SessionMem<1>> = StaticCell::new();
    let session_mem = SESSION_MEM.init(SessionMem::new());

    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(
        MultiprotocolServiceLayer::with_timeslots::<typelevel::EGU0_SWI0, _, 1>(
            mpsl_p,
            Irqs,
            lfclk_cfg,
            session_mem,
        )
        .unwrap(),
    );

    spawner.spawn(mpsl_task(mpsl).unwrap());
    spawner.spawn(hfclk_task(mpsl).unwrap());

    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("embassy-nrf-esb");
    config.product = Some("MPSL PRX in slot");

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

    {
        let mut buf = [0u8; 64];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(w, "PRX-in-slot ready\r\n");
        for chunk in w.bytes().chunks(64) {
            let _ = class.write_packet(chunk).await;
        }
    }

    embassy_time::Timer::after_secs(2).await;

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    // Run PRX in chained timeslots, 1000 slots per batch (~6 seconds).
    // Loop forever, reporting stats after each batch.
    let mut total_rx: u32 = 0;
    let mut total_dup: u32 = 0;
    let mut total_crc: u32 = 0;
    let mut batch: u32 = 0;

    loop {
        let r = run_prx_slots(mpsl, &esb_cfg, &esb_addr, 6000, 5500, 1000, 0x01).await;

        total_rx += r.rx_count;
        total_dup += r.dup_count;
        total_crc += r.bad_crc_count;
        batch += 1;

        // Guard all writes behind DTR check so a disconnected host never
        // causes write_packet to block indefinitely (the USB IN endpoint
        // would NAK forever with no consumer).
        if class.dtr() {
            let mut buf = [0u8; 256];
            let mut w = WriteBuf::new(&mut buf);
            let _ = write!(
                w,
                "[{}] rx={} dup={} crc={} st={} t0={} rad={} blk={} | tot_rx={}\r\n",
                batch,
                r.rx_count,
                r.dup_count,
                r.bad_crc_count,
                r.counters.start,
                r.counters.timer0,
                r.counters.radio,
                r.counters.blocked,
                total_rx,
            );
            for chunk in w.bytes().chunks(64) {
                let _ = class.write_packet(chunk).await;
            }
        }
    }
}
