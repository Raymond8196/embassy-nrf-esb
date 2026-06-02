//! Three-mode event-driven PTX: ESB send-on-event + BLE + USB CDC.
//!
//! Unlike the continuous-poll `mpsl_3mode_poll`, this example sends ESB
//! packets only when a simulated key event triggers. Each send attempts
//! one timeslot; if ACK is not received, it retries after a short delay.
//!
//! Pair with `mpsl_3mode_central` running on the PRX/main dongle.
//!
//! Flash:
//!   cargo build --example mpsl_3mode_event --features nrf52840,defmt,mpsl --release

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
use embassy_nrf_esb::mpsl_timeslot::{
    CoexistenceProfile, PtxEventConfig, open_event_session,
};

type Rng = rng::Rng<'static, embassy_nrf::mode::Blocking>;
type MyUsbDriver = UsbDriver<'static, &'static SoftwareVbusDetect>;

const LOG_BUF_SIZE: usize = 256;
const MAX_RETRIES: u8 = 5;
const RETRY_DELAY_MS: u64 = 2;

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

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

#[embassy_executor::task]
async fn hfclk_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    let _hfclk = mpsl.request_hfclk().await.unwrap();
    core::future::pending().await
}

#[embassy_executor::task]
async fn sdc_task(sdc: &'static SoftdeviceController<'static>) -> ! {
    let mut evt_buf = [0u8; sdc::raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
    loop {
        match sdc.hci_get(&mut evt_buf).await {
            Ok(bt_hci::PacketKind::AclData) => handle_acl(sdc, &evt_buf),
            Ok(bt_hci::PacketKind::Event) => handle_hci_event(sdc, &evt_buf).await,
            Ok(_) => {}
            Err(e) => defmt::warn!("sdc error: {:?}", e),
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
    let _ = cdc.write_packet(b"[EVENT] CDC connected\r\n").await;
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

async fn handle_hci_event(sdc: &SoftdeviceController<'_>, buf: &[u8]) {
    if buf.len() < 2 {
        return;
    }
    if buf[0] != 0x3e {
        return;
    }
    let event_len = buf[1] as usize;
    if buf.len() < 2 + event_len || event_len < 2 {
        return;
    }
    let data = &buf[2..2 + event_len];
    if data[1] == 0 && (data[0] == 1 || data[0] == 10) && data.len() >= 4 {
        let handle = u16::from_le_bytes([data[2], data[3]]) & 0x0fff;
        defmt::info!("BLE connected handle={}", handle);
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
        0x03 => {}
        0x10 => {
            let name = b"ESB Event";
            let mut resp = [0u8; 64];
            resp[0] = 0x11;
            resp[1] = pdu.get(1).copied().unwrap_or(0);
            resp[2] = pdu.get(2).copied().unwrap_or(0);
            let end = 3 + name.len().min(61);
            resp[3..end].copy_from_slice(&name[..end - 3]);
            send_l2cap(sdc, handle, 0x0004, &resp[..end]);
        }
        0x52 => {
            let resp = [0x13, pdu.get(1).copied().unwrap_or(0), 0x06];
            send_l2cap(sdc, handle, 0x0004, &resp);
        }
        _ => {}
    }
}

fn handle_l2cap_control(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }
    if pdu[0] == 0x02 && pdu.len() >= 4 {
        let resp = [0x03, pdu[1], pdu[2], pdu[3], 0x04, 0x00, 0x01, 0x00];
        send_l2cap(sdc, handle, 0x0005, &resp);
    }
    if pdu[0] == 0x06 && pdu.len() >= 2 {
        let resp = [0x07, pdu[1]];
        send_l2cap(sdc, handle, 0x0005, &resp);
    }
}

fn handle_smp(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }
    if pdu[0] == 0x01 {
        let resp = [0x03, 0x00, 0x00, 0x00, 0x00];
        send_l2cap(sdc, handle, 0x0006, &resp);
    }
}

fn send_l2cap(sdc: &SoftdeviceController<'_>, handle: u16, cid: u16, payload: &[u8]) {
    let len = payload.len();
    if len > 20 {
        return;
    }
    let mut packet = [0u8; 31];
    packet[0..2].copy_from_slice(&(handle & 0x0fff).to_le_bytes());
    packet[2..4].copy_from_slice(&((len + 4) as u16).to_le_bytes());
    packet[4..6].copy_from_slice(&(len as u16).to_le_bytes());
    packet[6..8].copy_from_slice(&cid.to_le_bytes());
    packet[8..8 + len].copy_from_slice(payload);
    let _ = sdc.hci_data_put(&packet[..8 + len]);
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

async fn start_advertising(sdc: &SoftdeviceController<'_>) {
    SetEventMask::new(
        EventMask::new()
            .enable_le_meta(true)
            .enable_disconnection_complete(true),
    )
    .exec(sdc)
    .await
    .unwrap();
    LeSetEventMask::new(
        LeEventMask::new()
            .enable_le_conn_complete(true)
            .enable_le_enhanced_conn_complete(true),
    )
    .exec(sdc)
    .await
    .unwrap();
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
        0x02, 0x01, 0x06, 0x08, 0x09, b'E', b'S', b'B', b'E', b'v', b't',
    ];
    let mut data = [0u8; 31];
    data[..adv_data.len()].copy_from_slice(adv_data);
    LeSetAdvData::new(adv_data.len() as u8, data)
        .exec(sdc)
        .await
        .unwrap();
    LeSetAdvEnable::new(true).exec(sdc).await.unwrap();
}

// ---- Main ----

const PROFILE: CoexistenceProfile = CoexistenceProfile::DiagnosticPipe1Prx12ms;

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

    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    static RNG: StaticCell<Rng> = StaticCell::new();
    let rng = RNG.init(rng::Rng::new_blocking(p.RNG));
    static SDC_MEM: StaticCell<sdc::Mem<8192>> = StaticCell::new();
    static SDC: StaticCell<SoftdeviceController> = StaticCell::new();
    let sdc = SDC.init(
        sdc::Builder::new()
            .unwrap()
            .support_adv()
            .support_peripheral()
            .peripheral_count(1)
            .unwrap()
            .build(sdc_p, rng, mpsl, SDC_MEM.init(sdc::Mem::new()))
            .unwrap(),
    );
    start_advertising(sdc).await;
    spawner.spawn(sdc_task(sdc).unwrap());
    defmt::info!("BLE advertising as 'ESBEvt'");

    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);
    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0004);
    usb_config.manufacturer = Some("ESB Event");
    usb_config.product = Some("3Mode Event");
    usb_config.max_power = 100;
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

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    let event_cfg = PtxEventConfig::for_profile(PROFILE);

    let mut session = match open_event_session(&mpsl, &esb_cfg, &esb_addr, event_cfg) {
        Ok(s) => s,
        Err(e) => {
            defmt::error!("open_event_session: {:?}", e);
            core::future::pending::<()>().await;
            unreachable!()
        }
    };

    defmt::info!("Event PTX session opened");
    log(b"[EVENT] session opened\r\n");

    let mut event_count: u32 = 0;
    let mut total_acked: u32 = 0;
    let mut total_sends: u32 = 0;
    let mut pending: Option<[u8; 4]> = None;

    loop {
        embassy_time::Timer::after_millis(50).await;

        if pending.is_none() {
            event_count += 1;
            pending = Some([
                (event_count & 0xFF) as u8,
                ((event_count >> 8) & 0xFF) as u8,
                0,
                0,
            ]);
        }

        let report = pending.unwrap();
        let mut acked = false;
        let mut attempts: u8 = 0;

        loop {
            attempts += 1;
            total_sends += 1;

            let result = match session.send(&report).await {
                Ok(r) => r,
                Err(e) => {
                    defmt::warn!("send error: {:?}", e);
                    log(b"[ERR] send\r\n");
                    break;
                }
            };

            if result.ack_ok {
                acked = true;
                break;
            }

            if attempts >= MAX_RETRIES {
                break;
            }

            embassy_time::Timer::after_millis(RETRY_DELAY_MS).await;
        }

        if acked {
            total_acked += 1;
            pending = None;
        }

        let mut buf = [0u8; LOG_BUF_SIZE];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "e={} ok={} att={} pend={}\r\n",
            event_count,
            acked,
            attempts,
            pending.is_some(),
        );
        let log_len = w.pos;
        log(&buf[..log_len]);

        if event_count % 20 == 0 {
            let mut w2 = WriteBuf::new(&mut buf);
            let _ = write!(
                w2,
                "[CUM] events={} sends={} acked={} rate={:.4}\r\n",
                event_count,
                total_sends,
                total_acked,
                total_acked as f64 / event_count as f64,
            );
            let cum_len = w2.pos;
            log(&buf[..cum_len]);
        }
    }
}
