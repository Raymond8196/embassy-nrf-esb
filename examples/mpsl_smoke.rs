//! M10 Step 0: MPSL initialization smoke test (USB CDC variant for dongle).
//!
//! No timeslot, no ESB. Brings up the Multiprotocol Service Layer, requests
//! HFCLK (needed for USB), exposes a USB CDC ACM port, and writes the MPSL
//! build revision + on-die temperature every 5 s.
//!
//! Run on dongle without SWD probe — open the resulting USB serial port at
//! any baud, e.g.:
//!     screen /dev/cu.usbmodemXXXX
//!
//! Pass criteria (docs/m10-plan.md Step 0):
//!   - "MPSL build revision: <hex>" line appears once (16 non-zero bytes).
//!   - "tick N: temp = <20..35> C" repeats every 5 s.
//!   - 30 min continuous run with no panic.
//!
//! IRQ ownership note: MPSL claims CLOCK_POWER for its clock subsystem, so
//! we use SoftwareVbusDetect(true, true) for USB. HFCLK is requested via
//! MPSL (instead of the pac::CLOCK shortcut used in usb_minimal) and held
//! for the lifetime of main.

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::UsbDevice;
use nrf_mpsl::{raw, MultiprotocolServiceLayer, Peripherals};
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
    device.run().await;
}

#[embassy_executor::task]
async fn hfclk_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    // Hold HFCLK forever for USB. nrf-mpsl's Hfclk guard releases on drop —
    // by leaking it via an infinite-await task we keep the clock on.
    let _hfclk = mpsl.request_hfclk().await.unwrap();
    core::future::pending().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    // LFCLK: internal RC, 500 ppm (matches nrf-mpsl doc example).
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

    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl =
        MPSL.init(MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk_cfg).unwrap());

    spawner.spawn(mpsl_task(mpsl).unwrap());
    spawner.spawn(hfclk_task(mpsl).unwrap());

    // USB: SoftwareVbusDetect — dongle is bus-powered, mark always-on.
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("embassy-nrf-esb");
    config.product = Some("MPSL smoke");

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

    // Print build revision once.
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

    let mut tick: u32 = 0;
    loop {
        let t = mpsl.get_temperature();
        let mut buf = [0u8; 128];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "tick {}: temp = {}.{:03} C (raw {})\r\n",
            tick,
            t.degrees(),
            t.millidegrees().unsigned_abs(),
            t.raw(),
        );

        for chunk in w.bytes().chunks(64) {
            let _ = class.write_packet(chunk).await;
        }

        tick = tick.wrapping_add(1);
        embassy_time::Timer::after_secs(5).await;
    }
}
