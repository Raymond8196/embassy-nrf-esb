//! M10 Step 5: PRX reception inside MPSL timeslots.
//!
//! Each 6 ms timeslot power-cycles the RADIO, inits ESB PRX registers,
//! listens for incoming packets, and sends ACKs. Runs chained slots
//! continuously, forever.
//!
//! No debug output: ground truth is the matched PTX example's
//! `ack_ok_count` over USB CDC. If PTX reports `tx == ack`, every PRX
//! timeslot received and acknowledged. Adding USB CDC here previously
//! exposed an embassy-usb 0.6 bug where `write_packet` is not
//! cancellation-safe and has no abort/disconnect API; eliminating the
//! dependency is cleaner than working around it.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::bind_interrupts;
use embassy_nrf::interrupt::typelevel;
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_timeslot::run_prx_slots;

bind_interrupts!(struct Irqs {
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

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    // Single long-lived PRX session; never re-enter so per-pipe ack_counter
    // stays monotonic for the entire run.
    let _ = run_prx_slots(mpsl, &esb_cfg, &esb_addr, 14000, 13500, u32::MAX, 0x03)
        .await
        .unwrap();
    loop {
        embassy_time::Timer::after_secs(60).await;
    }
}
