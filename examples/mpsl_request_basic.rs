//! M10 Step 1: Single timeslot request + callback signal log (USB CDC).
//!
//! Opens an MPSL session, requests one 5 ms EARLIEST timeslot at a time,
//! arms TIMER0 CC[0] at 4.5 ms to end the slot. Loops 100 times and
//! prints per-run signal counters. No ESB.
//!
//! Pass criteria (docs/m10-plan.md Step 1):
//!   - Per run: start=1 timer0=1 radio=0 idle=1 blocked=0 cancelled=0.
//!   - 100/100 runs complete successfully.

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::mpsl_timeslot::run_single_slot;

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
async fn usb_task(mut device: UsbDevice<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>) {
    device.run().await;
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

    let mpsl_p = Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);

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

    // USB CDC
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("embassy-nrf-esb");
    config.product = Some("MPSL timeslot basic");

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

    // Print build revision.
    {
        let mut buf = [0u8; 128];
        let mut w = WriteBuf::new(&mut buf);
        match MultiprotocolServiceLayer::build_revision() {
            Ok(rev) => {
                let _ = write!(w, "MPSL build revision: ");
                for b in &rev {
                    let _ = write!(w, "{:02x}", b);
                }
                let _ = write!(w, "\r\n");
            }
            Err(_) => {
                let _ = write!(w, "MPSL build_revision failed\r\n");
            }
        }
        for chunk in w.bytes().chunks(64) {
            let _ = class.write_packet(chunk).await;
        }
    }

    let mut ok_count: u32 = 0;
    let mut err_count: u32 = 0;

    for run in 0..100u32 {
        let c = run_single_slot(mpsl, 5000, 4500).await.unwrap();

        let mut buf = [0u8; 256];
        let mut w = WriteBuf::new(&mut buf);
        let ok = c.start == 1
            && c.timer0 == 1
            && c.radio == 0
            && c.blocked == 0
            && c.cancelled == 0
            && c.session_idle >= 1;
        if ok {
            ok_count += 1;
        } else {
            err_count += 1;
        }
        let _ = write!(
            w,
            "run {}: st={} t0={} radio={} idle={} blk={} can={} {}\r\n",
            run,
            c.start,
            c.timer0,
            c.radio,
            c.session_idle,
            c.blocked,
            c.cancelled,
            if ok { "OK" } else { "FAIL" }
        );

        for chunk in w.bytes().chunks(64) {
            let _ = class.write_packet(chunk).await;
        }
    }

    // Summary
    {
        let mut buf = [0u8; 128];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(w, "DONE ok={} err={}\r\n", ok_count, err_count);
        for chunk in w.bytes().chunks(64) {
            let _ = class.write_packet(chunk).await;
        }
    }

    // Keep running so USB stays alive for reading.
    loop {
        embassy_time::Timer::after_secs(60).await;
    }
}
