//! M10 Step 7 first pass: PRX-in-timeslot plus nrf-sdc BLE advertising.
//!
//! This is intentionally smaller than the final Step 7 target. It proves the
//! same MPSL instance can service both ESB PRX timeslots and the Nordic
//! SoftDevice Controller. Use nRF Connect to scan for the `ESB M10` advertiser
//! while a second board runs `mpsl_ptx_in_slot`.

#![no_std]
#![no_main]

use bt_hci::cmd::SyncCmd;
use bt_hci::cmd::le::{LeSetAdvData, LeSetAdvEnable, LeSetAdvParams};
use bt_hci::param::BdAddr;
use embassy_executor::Spawner;
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::mode::Blocking;
use embassy_nrf::{bind_interrupts, pac, peripherals, rng};
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use nrf_sdc::vendor::ZephyrWriteBdAddr;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_timeslot::run_prx_slots;

type Rng = rng::Rng<'static, Blocking>;

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<peripherals::RNG>;
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
async fn sdc_task(sdc: &'static SoftdeviceController<'static>) -> ! {
    let mut evt_buf = [0u8; sdc::raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
    loop {
        let _ = sdc.hci_get(&mut evt_buf).await;
    }
}

fn bd_addr() -> BdAddr {
    let ficr = pac::FICR;
    let addr = (u64::from(ficr.deviceid(1).read()) << 32) | u64::from(ficr.deviceid(0).read());
    BdAddr::new(
        ((addr | 0x0000_c000_0000_0000).to_le_bytes()[..6])
            .try_into()
            .unwrap(),
    )
}

fn build_sdc<'d, const N: usize>(
    p: sdc::Peripherals<'d>,
    rng: &'d mut Rng,
    mpsl: &'d MultiprotocolServiceLayer<'d>,
    mem: &'d mut sdc::Mem<N>,
) -> Result<sdc::SoftdeviceController<'d>, sdc::Error> {
    sdc::Builder::new()?.support_adv().build(p, rng, mpsl, mem)
}

async fn start_advertising(sdc: &SoftdeviceController<'_>) {
    ZephyrWriteBdAddr::new(bd_addr()).exec(sdc).await.unwrap();

    LeSetAdvParams::new(
        bt_hci::param::Duration::from_millis(100),
        bt_hci::param::Duration::from_millis(100),
        bt_hci::param::AdvKind::AdvScanInd,
        bt_hci::param::AddrKind::PUBLIC,
        bt_hci::param::AddrKind::PUBLIC,
        BdAddr::default(),
        bt_hci::param::AdvChannelMap::ALL,
        bt_hci::param::AdvFilterPolicy::default(),
    )
    .exec(sdc)
    .await
    .unwrap();

    let adv_data = &[
        0x02, 0x01, 0x06, // Flags: LE general discoverable, BR/EDR unsupported.
        0x08, 0x09, b'E', b'S', b'B', b' ', b'M', b'1', b'0', // Complete name.
    ];
    let mut data = [0u8; 31];
    data[..adv_data.len()].copy_from_slice(adv_data);
    LeSetAdvData::new(adv_data.len() as u8, data)
        .exec(sdc)
        .await
        .unwrap();
    LeSetAdvEnable::new(true).exec(sdc).await.unwrap();
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

    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );

    static RNG: StaticCell<Rng> = StaticCell::new();
    let rng = RNG.init(rng::Rng::new_blocking(p.RNG));

    static SDC_MEM: StaticCell<sdc::Mem<4096>> = StaticCell::new();
    static SDC: StaticCell<SoftdeviceController> = StaticCell::new();
    let sdc = SDC.init(build_sdc(sdc_p, rng, mpsl, SDC_MEM.init(sdc::Mem::new())).unwrap());

    start_advertising(sdc).await;
    spawner.spawn(sdc_task(sdc).unwrap());
    defmt::info!("BLE advertising started; entering ESB PRX timeslots");

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    let r = run_prx_slots(mpsl, &esb_cfg, &esb_addr, 14000, 13500, u32::MAX, 0x03).await;
    defmt::warn!("PRX timeslot session ended unexpectedly: {:?}", r);
}
