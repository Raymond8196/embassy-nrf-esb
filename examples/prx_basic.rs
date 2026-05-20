#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::peripherals::TIMER1;
use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPrx};
use embassy_nrf_esb::pac;
use embassy_nrf_esb::payload::PacketPool;
use {defmt_rtt as _, panic_probe as _};

mod interrupt {
    pub use embassy_nrf_esb::pac::Interrupt::*;
}

static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();
static mut PRX_REF: Option<&'static EsbPrx<TIMER1>> = None;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let config = EsbConfig::default();
    let addresses = EsbAddresses::default();

    let prx = {
        static ESB: static_cell::StaticCell<EsbPrx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPrx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses).unwrap())
    };
    unsafe { PRX_REF = Some(prx) };

    let radio = pac::RADIO;
    defmt::info!("ESB init ok");
    defmt::info!("  FREQUENCY  = {}", radio.frequency().read().frequency());
    defmt::info!("  MODE       = {:#010x}", radio.mode().read().0);
    defmt::info!("  PCNF0      = {:#010x}", radio.pcnf0().read().0);
    defmt::info!("  PCNF1      = {:#010x}", radio.pcnf1().read().0);
    defmt::info!("  CRCCNF     = {:#010x}", radio.crccnf().read().0);
    defmt::info!("  CRCPOLY    = {:#010x}", radio.crcpoly().read().0);
    defmt::info!("  CRCINIT    = {:#010x}", radio.crcinit().read().0);
    defmt::info!("  BASE0      = {:#010x}", radio.base0().read());
    defmt::info!("  PREFIX0    = {:#010x}", radio.prefix0().read().0);
    defmt::info!("  TXPOWER    = {:#010x}", radio.txpower().read().0);

    prx.start_listening().expect("start_listening failed");
    defmt::info!("PRX listening...");

    let mut rx_count: u32 = 0;
    loop {
        let pkt = prx.receive().await;
        rx_count += 1;
        defmt::info!(
            "[RX] #{}: pipe={} len={} data={=[u8]:x}",
            rx_count,
            pkt.pipe(),
            pkt.len(),
            pkt.payload()
        );
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
