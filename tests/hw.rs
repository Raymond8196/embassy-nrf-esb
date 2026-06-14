//! On-target register HIL tests.
//!
//! Verifies that constructing the public `EsbPrx` driver programs the RADIO and
//! TIMER peripherals exactly as `EsbConfig::default()` / `EsbAddresses::default()`
//! require. These run on real hardware (Elytra nRF52833 over SWD) via
//! `cargo xtask test-hw`; the nRF52840 dongles are DFU-only and cannot be driven
//! by probe-rs / embedded-test.
//!
//! embedded-test provides the test runner and the panic handler, so this file
//! intentionally does not pull `panic-probe`.

#![no_std]
#![no_main]

use defmt_rtt as _;

#[embedded_test::tests]
mod tests {
    use embassy_nrf::pac;
    use embassy_nrf::peripherals::TIMER1;
    use embassy_nrf_esb::addresses::EsbAddresses;
    use embassy_nrf_esb::config::EsbConfig;
    use embassy_nrf_esb::isr::{DEFAULT_POOL_N, DEFAULT_POOL_SIZE, EsbPrx};
    use embassy_nrf_esb::payload::PacketPool;

    static POOL: PacketPool<DEFAULT_POOL_N, DEFAULT_POOL_SIZE> = PacketPool::new();

    /// Construct the real public PRX driver with default config/addresses on
    /// TIMER1, then assert the RADIO and TIMER1 registers match the values
    /// `radio.rs`/`timer.rs` are documented to write. A single test (one
    /// `embassy_nrf::init`) keeps this independent of embedded-test's
    /// between-test reset behavior.
    #[test]
    fn prx_default_init_programs_radio_and_timer_registers() {
        let p = embassy_nrf::init(Default::default());

        let config = EsbConfig::default();
        let addresses = EsbAddresses::default();

        // Side effect of construction: RADIO + TIMER1 are fully programmed.
        // Keep the driver alive until all asserts run.
        let _prx =
            EsbPrx::<TIMER1>::new(p.TIMER1, p.RADIO, &POOL, &config, &addresses).unwrap();

        let r = pac::RADIO;

        // Bitrate: default Mbps2 -> NRF_2MBIT.
        assert_eq!(r.mode().read().mode(), pac::radio::vals::Mode::NRF_2MBIT);

        // PCNF0: 8-bit LENGTH field, 3-bit S1 (PID + NO_ACK).
        let pcnf0 = r.pcnf0().read();
        assert_eq!(pcnf0.lflen(), 8);
        assert_eq!(pcnf0.s1len(), 3);

        // PCNF1: MAXLEN mirrors payload_length (default 32), 4-byte base, big-endian.
        let pcnf1 = r.pcnf1().read();
        assert_eq!(pcnf1.maxlen(), config.payload_length);
        assert_eq!(pcnf1.balen(), 4);
        assert_eq!(pcnf1.statlen(), 0);
        assert_eq!(pcnf1.endian(), pac::radio::vals::Endian::BIG);

        // CRC: 16-bit, init 0xFFFF, polynomial 0x11021 (explicit MSB), address included.
        let crccnf = r.crccnf().read();
        assert_eq!(crccnf.len(), pac::radio::vals::Len::TWO);
        assert_eq!(crccnf.skipaddr(), pac::radio::vals::Skipaddr::INCLUDE);
        assert_eq!(r.crcinit().read().crcinit(), 0xFFFF);
        assert_eq!(r.crcpoly().read().crcpoly(), 0x11021);

        // Shorts configured by init.
        let shorts = r.shorts().read();
        assert!(shorts.ready_start());
        assert!(shorts.end_disable());
        assert!(shorts.address_rssistart());
        assert!(shorts.disabled_rssistop());

        // RF channel: default 2.
        assert_eq!(r.frequency().read().frequency(), 2);

        // TX power: default 0 dBm.
        assert_eq!(r.txpower().read().txpower(), pac::radio::vals::Txpower::_0_DBM);

        // TIMER1: 32-bit mode, prescaler 4 (1 MHz / 1 µs tick).
        let t = pac::TIMER1;
        assert_eq!(t.bitmode().read().bitmode(), pac::timer::vals::Bitmode::_32BIT);
        assert_eq!(t.prescaler().read().prescaler(), 4);
    }
}
