//! M10 Step 7 first pass: PRX-in-timeslot plus nrf-sdc BLE advertising.
//!
//! This is intentionally smaller than the final Step 7 target. It proves the
//! same MPSL instance can service both ESB PRX timeslots and the Nordic
//! SoftDevice Controller. Use nRF Connect to scan for the `ESB M10` advertiser
//! while a second board runs `mpsl_ptx_in_slot`.

#![no_std]
#![no_main]

use bt_hci::cmd::controller_baseband::SetEventMask;
use bt_hci::cmd::le::{LeConnUpdate, LeSetAdvData, LeSetAdvEnable, LeSetAdvParams, LeSetEventMask};
use bt_hci::cmd::{AsyncCmd, SyncCmd};
use bt_hci::param::{
    AdvChannelMap, AdvFilterPolicy, AdvKind, BdAddr, ConnHandle, EventMask, LeEventMask,
};
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
        match sdc.hci_get(&mut evt_buf).await {
            Ok(bt_hci::PacketKind::AclData) => handle_acl(sdc, &evt_buf),
            Ok(bt_hci::PacketKind::Event) => handle_hci_event(sdc, &evt_buf).await,
            Ok(_) => {}
            Err(e) => defmt::warn!("sdc hci_get error: {:?}", e),
        }
    }
}

async fn handle_hci_event(sdc: &SoftdeviceController<'_>, buf: &[u8]) {
    if buf.len() < 2 {
        return;
    }

    let event_code = buf[0];
    let event_len = buf[1] as usize;
    if event_code != 0x3e || buf.len() < 2 + event_len || event_len < 2 {
        return;
    }

    let data = &buf[2..2 + event_len];
    let subevent = data[0];
    let status = data[1];
    if status == 0 && (subevent == 1 || subevent == 10) && data.len() >= 4 {
        let handle = u16::from_le_bytes([data[2], data[3]]) & 0x0fff;
        defmt::info!(
            "BLE connected; requesting relaxed conn params handle={}",
            handle
        );
        request_relaxed_conn_params(sdc, handle).await;
    }
}

async fn request_relaxed_conn_params(sdc: &SoftdeviceController<'_>, handle: u16) {
    let _ = LeConnUpdate::new(
        ConnHandle::new(handle),
        bt_hci::param::Duration::from_millis(100),
        bt_hci::param::Duration::from_millis(100),
        4,
        bt_hci::param::Duration::from_millis(6000),
        bt_hci::param::Duration::from_millis(0),
        bt_hci::param::Duration::from_millis(0),
    )
    .exec(sdc)
    .await;
}

fn handle_acl(sdc: &SoftdeviceController<'_>, buf: &[u8]) {
    if buf.len() < 8 {
        return;
    }

    let handle = u16::from_le_bytes([buf[0], buf[1]]) & 0x0fff;
    let acl_len = u16::from_le_bytes([buf[2], buf[3]]) as usize;
    if acl_len < 4 || buf.len() < 4 + acl_len {
        return;
    }

    let l2cap_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    let cid = u16::from_le_bytes([buf[6], buf[7]]);
    if buf.len() < 8 + l2cap_len {
        return;
    }

    let payload = &buf[8..8 + l2cap_len];
    match cid {
        0x0004 => handle_att(sdc, handle, payload),
        0x0005 => handle_l2cap_control(sdc, handle, payload),
        0x0006 => handle_smp(sdc, handle, payload),
        _ => {}
    }
}

fn handle_att(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }

    match pdu[0] {
        0x02 => send_l2cap(sdc, handle, 0x0004, &[0x03, 23, 0]),
        0x04 => handle_find_information(sdc, handle, pdu),
        0x08 => handle_read_by_type(sdc, handle, pdu),
        0x0a if pdu.len() >= 3 && u16::from_le_bytes([pdu[1], pdu[2]]) == 3 => {
            send_l2cap(sdc, handle, 0x0004, b"\x0bESB M10");
        }
        0x10 => handle_read_by_group_type(sdc, handle, pdu),
        0x12 => send_l2cap(sdc, handle, 0x0004, &[0x13]),
        opcode => send_att_error(sdc, handle, opcode, req_handle(pdu), 0x06),
    }
}

fn handle_find_information(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let Some((start, end)) = att_range(pdu) else {
        send_att_error(sdc, handle, 0x04, 0, 0x04);
        return;
    };

    if start <= 2 && end >= 2 {
        send_l2cap(sdc, handle, 0x0004, &[0x05, 0x01, 2, 0, 0x03, 0x28]);
    } else if start <= 3 && end >= 3 {
        send_l2cap(sdc, handle, 0x0004, &[0x05, 0x01, 3, 0, 0x00, 0x2a]);
    } else {
        send_att_error(sdc, handle, 0x04, start, 0x0a);
    }
}

fn handle_read_by_type(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let Some((start, end)) = att_range(pdu) else {
        send_att_error(sdc, handle, 0x08, 0, 0x04);
        return;
    };

    if pdu.len() >= 7 && pdu[5] == 0x03 && pdu[6] == 0x28 && start <= 2 && end >= 2 {
        send_l2cap(
            sdc,
            handle,
            0x0004,
            &[0x09, 7, 2, 0, 0x02, 3, 0, 0x00, 0x2a],
        );
    } else if pdu.len() >= 7 && pdu[5] == 0x00 && pdu[6] == 0x2a && start <= 3 && end >= 3 {
        send_l2cap(sdc, handle, 0x0004, b"\x09\x0a\x03\x00ESB M10");
    } else {
        send_att_error(sdc, handle, 0x08, start, 0x0a);
    }
}

fn handle_read_by_group_type(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    let Some((start, end)) = att_range(pdu) else {
        send_att_error(sdc, handle, 0x10, 0, 0x04);
        return;
    };

    if pdu.len() >= 7 && pdu[5] == 0x00 && pdu[6] == 0x28 && start <= 1 && end >= 1 {
        send_l2cap(sdc, handle, 0x0004, &[0x11, 6, 1, 0, 5, 0, 0x00, 0x18]);
    } else {
        send_att_error(sdc, handle, 0x10, start, 0x0a);
    }
}

fn handle_l2cap_control(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.len() < 4 {
        return;
    }

    let code = pdu[0];
    let ident = pdu[1];
    let len = u16::from_le_bytes([pdu[2], pdu[3]]) as usize;
    if pdu.len() < 4 + len {
        return;
    }

    match code {
        0x12 => send_l2cap(sdc, handle, 0x0005, &[0x13, ident, 2, 0, 0, 0]),
        _ => send_l2cap(sdc, handle, 0x0005, &[0x01, ident, 2, 0, code, 0]),
    }
}

fn handle_smp(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if !pdu.is_empty() && pdu[0] == 0x01 {
        send_l2cap(sdc, handle, 0x0006, &[0x05, 0x05]);
    }
}

fn req_handle(pdu: &[u8]) -> u16 {
    if pdu.len() >= 3 {
        u16::from_le_bytes([pdu[1], pdu[2]])
    } else {
        0
    }
}

fn att_range(pdu: &[u8]) -> Option<(u16, u16)> {
    if pdu.len() >= 5 {
        Some((
            u16::from_le_bytes([pdu[1], pdu[2]]),
            u16::from_le_bytes([pdu[3], pdu[4]]),
        ))
    } else {
        None
    }
}

fn send_att_error(sdc: &SoftdeviceController<'_>, handle: u16, req: u8, attr: u16, err: u8) {
    let [lo, hi] = attr.to_le_bytes();
    send_l2cap(sdc, handle, 0x0004, &[0x01, req, lo, hi, err]);
}

fn send_l2cap(sdc: &SoftdeviceController<'_>, handle: u16, cid: u16, payload: &[u8]) {
    let len = payload.len();
    if len > 23 {
        return;
    }

    let mut packet = [0u8; 31];
    let handle_pb = handle & 0x0fff;
    packet[0..2].copy_from_slice(&handle_pb.to_le_bytes());
    packet[2..4].copy_from_slice(&((len + 4) as u16).to_le_bytes());
    packet[4..6].copy_from_slice(&(len as u16).to_le_bytes());
    packet[6..8].copy_from_slice(&cid.to_le_bytes());
    packet[8..8 + len].copy_from_slice(payload);

    if let Err(e) = sdc.hci_data_put(&packet[..8 + len]) {
        defmt::warn!("hci_data_put failed: {:?}", e);
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
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .peripheral_count(1)?
        .build(p, rng, mpsl, mem)
}

async fn start_advertising(sdc: &SoftdeviceController<'_>) {
    let event_mask = EventMask::new()
        .enable_le_meta(true)
        .enable_disconnection_complete(true);
    SetEventMask::new(event_mask).exec(sdc).await.unwrap();

    let le_event_mask = LeEventMask::new()
        .enable_le_conn_complete(true)
        .enable_le_enhanced_conn_complete(true)
        .enable_le_conn_update_complete(true)
        .enable_le_remote_conn_parameter_request(true);
    LeSetEventMask::new(le_event_mask).exec(sdc).await.unwrap();

    ZephyrWriteBdAddr::new(bd_addr()).exec(sdc).await.unwrap();

    LeSetAdvParams::new(
        bt_hci::param::Duration::from_millis(100),
        bt_hci::param::Duration::from_millis(100),
        AdvKind::AdvInd,
        bt_hci::param::AddrKind::PUBLIC,
        bt_hci::param::AddrKind::PUBLIC,
        BdAddr::default(),
        AdvChannelMap::ALL,
        AdvFilterPolicy::default(),
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

    static SDC_MEM: StaticCell<sdc::Mem<8192>> = StaticCell::new();
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

    let r = run_prx_slots(mpsl, &esb_cfg, &esb_addr, 14000, 13500, u32::MAX, 0x03)
        .await
        .unwrap();
    defmt::warn!("PRX timeslot session ended unexpectedly: {:?}", r);
}
