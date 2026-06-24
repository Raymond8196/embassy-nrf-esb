//! Three-mode polling central: PTX round-robin poll + BLE + USB CDC.
//!
//! Central polls up to 7 ESB peripherals (PRX) in round-robin via MPSL
//! timeslots, each slot sending 1 packet to 1 pipe and waiting for ACK.
//! Simultaneously runs BLE advertising/connectable and USB CDC log.
//!
//! Target polling rate:
//!   7 pipes × 1500µs slot = 10.5ms per round ≈ 95 Hz
//!
//! Pair with multiple boards running `mpsl_prx_in_slot`.
//!
//! Flash:
//!   cargo build --example mpsl_3mode_poll --features nrf52840,defmt,mpsl --release

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
use embassy_nrf_esb::mpsl_timeslot::{CoexistenceProfile, PtxPollConfig, open_ptx_poll_session};

type Rng = rng::Rng<'static, embassy_nrf::mode::Blocking>;
type MyUsbDriver = UsbDriver<'static, &'static SoftwareVbusDetect>;

const LOG_BUF_SIZE: usize = 256;
const POLL_REPORT_TIMEOUT_MS: u64 = 500;

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
    let _ = cdc.write_packet(b"[POLL] CDC connected\r\n").await;
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
        0x10 => {
            if pdu.len() >= 7 && pdu[5] == 0x00 && pdu[6] == 0x28 {
                send_l2cap(sdc, handle, 0x0004, &[0x11, 6, 1, 0, 5, 0, 0x00, 0x18]);
            } else {
                send_att_error(sdc, handle, 0x10, att_start(pdu), 0x0a);
            }
        }
        0x12 => send_l2cap(sdc, handle, 0x0004, &[0x13]),
        opcode => send_att_error(sdc, handle, opcode, att_handle(pdu), 0x06),
    }
}

fn handle_l2cap_control(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.len() < 4 {
        return;
    }
    let code = pdu[0];
    let ident = pdu[1];
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

fn att_handle(pdu: &[u8]) -> u16 {
    if pdu.len() >= 3 {
        u16::from_le_bytes([pdu[1], pdu[2]])
    } else {
        0
    }
}

fn att_start(pdu: &[u8]) -> u16 {
    att_handle(pdu)
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
            .enable_le_enhanced_conn_complete_v1(true),
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
        0x02, 0x01, 0x06, 0x09, 0x09, b'E', b'S', b'B', b'P', b'o', b'l', b'l',
    ];
    let mut data = [0u8; 31];
    data[..adv_data.len()].copy_from_slice(adv_data);
    LeSetAdvData::new(adv_data.len() as u8, data)
        .exec(sdc)
        .await
        .unwrap();
    LeSetAdvEnable::new(true).exec(sdc).await.unwrap();
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

const PROFILE: CoexistenceProfile = CoexistenceProfile::DiagnosticPipe5ScheduledGate;

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

    // BLE
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
    defmt::info!("BLE advertising as 'ESBPoll'");

    // USB CDC
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);
    let mut usb_config = embassy_usb::Config::new(0x1209, 0x0004);
    usb_config.manufacturer = Some("ESB Poll");
    usb_config.product = Some("3Mode Poll");
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

    // ESB poll session
    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    let poll_cfg = PtxPollConfig::for_profile(PROFILE);

    let mut poll = match open_ptx_poll_session(mpsl, &esb_cfg, &esb_addr, poll_cfg) {
        Ok(s) => s,
        Err(e) => {
            defmt::error!("open_ptx_poll_session: {:?}", e);
            core::future::pending::<()>().await;
            unreachable!()
        }
    };

    defmt::info!(
        "PTX poll session started ({} pipes, {}µs slot)",
        poll_cfg.pipe_count(),
        poll_cfg.slot_length_us
    );
    log(b"[POLL] started\r\n");

    let mut round: u32 = 0;
    let mut timeout_count: u32 = 0;
    loop {
        round += 1;
        let r = match embassy_time::with_timeout(
            embassy_time::Duration::from_millis(POLL_REPORT_TIMEOUT_MS),
            poll.next_report(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                defmt::warn!("poll error: {:?}", e);
                log(b"[ERR]\r\n");
                embassy_time::Timer::after_millis(100).await;
                continue;
            }
            Err(_) => {
                timeout_count += 1;
                defmt::warn!("poll report timeout; reopening session ({})", timeout_count);
                log(b"[TIMEOUT]\r\n");
                drop(poll);

                poll = loop {
                    match open_ptx_poll_session(mpsl, &esb_cfg, &esb_addr, poll_cfg) {
                        Ok(session) => {
                            log(b"[RECOVER]\r\n");
                            break session;
                        }
                        Err(e) => {
                            defmt::warn!("reopen poll session failed: {:?}", e);
                            log(b"[REOPEN_ERR]\r\n");
                            embassy_time::Timer::after_millis(100).await;
                        }
                    }
                };
                continue;
            }
        };

        let mut buf = [0u8; LOG_BUF_SIZE];
        let mut w = WriteBuf::new(&mut buf);
        let _ = write!(
            w,
            "r={} tx={} ack={} to={} crc={} sh={}/{} sid={} sj={}/{}/{}/{} ms={}/{} sk={} sl={}/{}/{} rq={} sp={} s={} t0={} rd={} dt={}",
            round,
            r.tx_count,
            r.ack_ok_count,
            r.ack_timeout_count,
            r.ack_crc_fail_count,
            r.schedule_hint_count,
            r.schedule_hint_bad_count,
            r.last_schedule_window_id,
            r.schedule.repeat_count,
            r.schedule.jump_count,
            r.schedule.regress_count,
            r.schedule.missed_window_count,
            r.schedule.current_miss_streak,
            r.schedule.max_miss_streak,
            r.schedule_skip_count,
            r.schedule_lock_active as u8,
            r.schedule_lock_count,
            r.schedule_lock_miss_streak,
            r.schedule_reacquire_count,
            r.schedule_period_us,
            r.counters.start,
            r.counters.timer0,
            r.counters.radio,
            r.counters.radio_disable_timeout,
        );
        for i in 0..8 {
            if poll_cfg.pipe_mask & (1 << i) != 0 {
                let _ = write!(
                    w,
                    " p{}:{}/{}/{}/{}",
                    i,
                    r.ack_per_pipe[i],
                    r.tx_per_pipe[i],
                    r.ack_timeout_per_pipe[i],
                    r.ack_crc_fail_per_pipe[i]
                );
            }
        }
        let _ = write!(w, "\r\n");
        let log_len = w.pos;
        log(&buf[..log_len]);

        if round % 50 == 0 {
            let mut w2 = WriteBuf::new(&mut buf);
            let _ = write!(
                w2,
                "[HZ] slots={} in ~{}ms\r\n",
                r.counters.start,
                r.counters.start * poll_cfg.slot_length_us / 1000,
            );
            let hz_len = w2.pos;
            log(&buf[..hz_len]);
        }
    }
}
