//! Three-mode central: ESB PRX-in-timeslot + BLE connectable + USB CDC.
//!
//! Demonstrates simultaneous operation of all three radio/transport modes:
//!   1. ESB PRX receives packets from PTX peripherals in MPSL timeslots
//!   2. BLE advertises as connectable peripheral ("ESB 3MODE")
//!   3. USB CDC outputs ESB statistics (optional, doesn't block ESB)
//!
//! Architecture:
//!   - Single MPSL instance shared by ESB timeslots and nrf-sdc BLE.
//!   - ESB starts immediately; BLE starts immediately.
//!   - USB CDC is fire-and-forget: writes are silently dropped until
//!     a host connects, after which statistics flow to the host.
//!   - BLE connection parameters are relaxed (CI=100ms, latency=4) to
//!     leave radio budget for ESB.
//!   - USB uses SoftwareVbusDetect because MPSL owns CLOCK_POWER.
//!
//! Pairing: flash `mpsl_ptx_continuous` on a second board.
//!
//! Flash:
//!   cargo build --example mpsl_3mode_central --features nrf52840,defmt,mpsl --release
//!   arm-none-eabi-objcopy -O ihex target/thumbv7em-none-eabihf/release/examples/mpsl_3mode_central mpsl_3mode_central.hex

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use bt_hci::cmd::controller_baseband::SetEventMask;
use bt_hci::cmd::le::{LeConnUpdate, LeSetAdvData, LeSetAdvEnable, LeSetAdvParams, LeSetEventMask};
use bt_hci::cmd::{AsyncCmd, SyncCmd};
use bt_hci::param::{
    AdvChannelMap, AdvFilterPolicy, AdvKind, BdAddr, ConnHandle, EventMask, LeEventMask,
};
use embassy_executor::Spawner;
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, rng, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State as CdcState};
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use nrf_sdc::vendor::ZephyrWriteBdAddr;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_timeslot::{PrxSlotResult, open_prx_session};

type Rng = rng::Rng<'static, embassy_nrf::mode::Blocking>;
type MyUsbDriver = UsbDriver<'static, &'static SoftwareVbusDetect>;

const LOG_BUF_SIZE: usize = 192;

static LOG_CHANNEL: Channel<CriticalSectionRawMutex, heapless::Vec<u8, LOG_BUF_SIZE>, 4> =
    Channel::new();

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    RNG => rng::InterruptHandler<peripherals::RNG>;
    EGU0_SWI0 => nrf_mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => nrf_mpsl::ClockInterruptHandler;
    RADIO => nrf_mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_mpsl::HighPrioInterruptHandler;
});

// ---- Tasks ----

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

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await
}

#[embassy_executor::task]
async fn cdc_logger_task(mut cdc: CdcAcmClass<'static, MyUsbDriver>) {
    cdc.wait_connection().await;
    let _ = cdc.write_packet(b"[3MODE] CDC connected\r\n").await;
    loop {
        let msg = LOG_CHANNEL.receive().await;
        for chunk in msg.chunks(64) {
            let _ = cdc.write_packet(chunk).await;
        }
    }
}

fn log(data: &[u8]) {
    let v = heapless::Vec::from_slice(data);
    if let Ok(v) = v {
        let _ = LOG_CHANNEL.try_send(v);
    }
}

// ---- BLE helpers ----

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
            send_l2cap(sdc, handle, 0x0004, b"\x0bESB 3MODE");
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
        send_l2cap(sdc, handle, 0x0004, b"\x09\x0c\x03\x00ESB 3MODE");
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
    let ficr = embassy_nrf::pac::FICR;
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
        0x02, 0x01, 0x06, 0x0a, 0x09, b'E', b'S', b'B', b' ', b'3', b'M', b'O', b'D', b'E',
    ];
    let mut data = [0u8; 31];
    data[..adv_data.len()].copy_from_slice(adv_data);
    LeSetAdvData::new(adv_data.len() as u8, data)
        .exec(sdc)
        .await
        .unwrap();
    LeSetAdvEnable::new(true).exec(sdc).await.unwrap();
}

// ---- Helpers ----

struct WriteBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> WriteBuf<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
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

fn format_prx(buf: &mut [u8], batch: u32, r: &PrxSlotResult) -> usize {
    let mut w = WriteBuf::new(buf);
    let _ = write!(
        w,
        "b={} rx={} dup={} crc={} p0={} p1={} s={} t0={} rd={} bk={} cn={}\r\n",
        batch,
        r.rx_count,
        r.dup_count,
        r.bad_crc_count,
        r.rx_per_pipe[0],
        r.rx_per_pipe[1],
        r.counters.start,
        r.counters.timer0,
        r.counters.radio,
        r.counters.blocked,
        r.counters.cancelled,
    );
    w.pos
}

// ---- Main ----

const BATCH_SIZE: u32 = 4;
const SLOT_US: u32 = 3000;
const MATCH_US: u32 = 2800;
const PIPES: u8 = 0x03;
const ESB_IDLE_MS: u64 = 0;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    // MPSL
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

    // BLE
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
    defmt::info!("BLE advertising as 'ESB 3MODE'");

    // USB CDC (fire-and-forget via channel, does not block ESB)
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);

    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0003);
    usb_config.manufacturer = Some("ESB 3Mode");
    usb_config.product = Some("Central");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static CDC_STATE: StaticCell<CdcState<'static>> = StaticCell::new();

    let mut builder = embassy_usb::Builder::new(
        driver,
        usb_config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let cdc_class = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), 64);
    let usb_dev = builder.build();
    spawner.spawn(usb_task(usb_dev).unwrap());
    spawner.spawn(cdc_logger_task(cdc_class).unwrap());

    // ESB starts immediately — no waiting for USB CDC
    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    defmt::info!("ESB PRX loop starting");
    log(b"[3MODE] ESB PRX + BLE + USB started\r\n");

    let mut batch: u32 = 0;
    let mut total_rx: u32 = 0;
    let mut total_p0: u32 = 0;
    let mut total_p1: u32 = 0;
    let mut total_dup: u32 = 0;
    let mut total_blk: u32 = 0;

    let mut err_count: u32 = 0;
    let mut prx_session = match open_prx_session(
        mpsl, &esb_cfg, &esb_addr, SLOT_US, MATCH_US, BATCH_SIZE, PIPES,
    ) {
        Ok(session) => session,
        Err(e) => {
            defmt::error!("open_prx_session failed: {:?}", e);
            let mut buf = [0u8; LOG_BUF_SIZE];
            let mut w = WriteBuf::new(&mut buf);
            let _ = write!(w, "[ERR] open_prx_session {:?}\r\n", e);
            let err_len = w.pos;
            log(&buf[..err_len]);
            core::future::pending().await
        }
    };

    loop {
        batch += 1;

        let result = prx_session.next_report().await;

        let r = match result {
            Ok(r) => r,
            Err(e) => {
                err_count += 1;
                defmt::warn!("prx_session error {:?} (count={})", e, err_count);
                let mut buf = [0u8; LOG_BUF_SIZE];
                let mut w = WriteBuf::new(&mut buf);
                let _ = write!(w, "[ERR] {:?} cnt={}\r\n", e, err_count);
                let err_len = w.pos;
                log(&buf[..err_len]);
                embassy_time::Timer::after_millis(100).await;
                continue;
            }
        };

        total_rx += r.rx_count;
        total_p0 += r.rx_per_pipe[0];
        total_p1 += r.rx_per_pipe[1];
        total_dup += r.dup_count;
        total_blk += r.counters.blocked;

        let mut buf = [0u8; LOG_BUF_SIZE];
        let len = format_prx(&mut buf, batch, &r);
        log(&buf[..len]);

        if batch % 20 == 0 {
            let mut w = WriteBuf::new(&mut buf);
            let _ = write!(
                w,
                "[CUM] rx={} p0={} p1={} dup={} blk={}\r\n",
                total_rx, total_p0, total_p1, total_dup, total_blk
            );
            let cum_len = w.pos;
            log(&buf[..cum_len]);
        }

        embassy_time::Timer::after_millis(ESB_IDLE_MS).await;
    }
}
