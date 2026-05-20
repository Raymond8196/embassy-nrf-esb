#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPtx};
use embassy_nrf::pac;
use embassy_nrf_esb::payload::PacketPool;
use {defmt_rtt as _, panic_probe as _};

mod interrupt {
    pub use embassy_nrf::pac::Interrupt::*;
}

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PTX_REF: Option<&'static EsbPtx<TIMER1>> = None;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    // RADIO requires external HFCLK
    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    let config = EsbConfig::default();
    let addresses = EsbAddresses::default();

    let ptx = {
        static ESB: static_cell::StaticCell<EsbPtx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPtx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses, 0).unwrap())
    };
    unsafe { PTX_REF = Some(ptx) };

    let mut counter: u32 = 0;

    loop {
        // Send 100 packets
        for _ in 0..100 {
            let payload = counter.to_le_bytes();
            let _ = ptx.send(&payload).await;
            counter = counter.wrapping_add(1);
            embassy_time::Timer::after_millis(10).await;
        }

        // Suspend ESB
        let saved = ptx.suspend().await;

        // Radio off for 500ms
        embassy_time::Timer::after_millis(500).await;

        // Restore and resume
        ptx.restore(&saved, &addresses);
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
