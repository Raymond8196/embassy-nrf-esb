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

    let config = EsbConfig::default();
    let addresses = EsbAddresses::default();

    let ptx = {
        static ESB: static_cell::StaticCell<EsbPtx<TIMER1>> = static_cell::StaticCell::new();
        &*ESB.init(EsbPtx::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses, 0).unwrap())
    };
    unsafe { PTX_REF = Some(ptx) };

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

    let mut counter: u32 = 0;
    loop {
        let payload = counter.to_le_bytes();
        match ptx.send(&payload).await {
            Ok(()) => {
                defmt::info!("[TX] #{}: sent ok", counter);
            }
            Err(e) => {
                defmt::warn!("[TX] #{}: error {:?}", counter, e);
            }
        }

        if ptx.max_attempts_reached() {
            defmt::warn!("[TX] max retransmit reached!");
        }

        if let Some(ack) = ptx.try_receive() {
            defmt::info!(
                "[TX] ACK payload: pipe={} len={} data={=[u8]:x}",
                ack.pipe(),
                ack.len(),
                ack.payload()
            );
        }

        counter = counter.wrapping_add(1);
        embassy_time::Timer::after_millis(500).await;
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
