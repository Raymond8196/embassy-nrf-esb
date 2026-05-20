//! M10 Step 7 diagnostic: nrf-sdc connectable advertising without ESB.
//!
//! This isolates BLE peripheral setup from ESB timeslot activity. Use nRF
//! Connect to scan for `ESB CONN` and connect. Defmt logs report the SDC memory
//! requirement, advertising enable status, and basic HCI connection events.

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use core::sync::atomic::{AtomicU16, Ordering};

use bt_hci::cmd::SyncCmd;
use bt_hci::cmd::controller_baseband::SetEventMask;
use bt_hci::cmd::le::{LeSetAdvData, LeSetAdvEnable, LeSetAdvParams, LeSetEventMask};
use bt_hci::event::EventPacket;
use bt_hci::param::{AdvChannelMap, AdvFilterPolicy, AdvKind, BdAddr, EventMask, LeEventMask};
use embassy_executor::Spawner;
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::mode::Blocking;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, pac, peripherals, rng, usb};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use nrf_sdc::vendor::ZephyrWriteBdAddr;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

type Rng = rng::Rng<'static, Blocking>;

static CONN_HANDLE: AtomicU16 = AtomicU16::new(0xffff);
static LOGS: Channel<ThreadModeRawMutex, LogLine, 16> = Channel::new();

#[derive(Clone, Copy)]
struct LogLine {
    len: usize,
    bytes: [u8; 96],
}

impl LogLine {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

struct WriteBuf {
    buf: [u8; 96],
    pos: usize,
}

impl WriteBuf {
    fn new() -> Self {
        Self {
            buf: [0; 96],
            pos: 0,
        }
    }

    fn finish(self) -> LogLine {
        LogLine {
            len: self.pos,
            bytes: self.buf,
        }
    }
}

impl core::fmt::Write for WriteBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = (self.pos + bytes.len()).min(self.buf.len());
        let count = end - self.pos;
        self.buf[self.pos..end].copy_from_slice(&bytes[..count]);
        self.pos = end;
        Ok(())
    }
}

macro_rules! usb_log {
    ($($arg:tt)*) => {{
        let mut w = WriteBuf::new();
        let _ = writeln!(w, $($arg)*);
        let _ = LOGS.try_send(w.finish());
    }};
}

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<peripherals::RNG>;
    USBD => usb::InterruptHandler<peripherals::USBD>;
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
async fn usb_task(mut device: UsbDevice<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>) {
    device.run().await
}

#[embassy_executor::task]
async fn log_task(
    mut class: CdcAcmClass<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>,
) {
    class.wait_connection().await;
    loop {
        let line = LOGS.receive().await;
        for chunk in line.as_slice().chunks(64) {
            let _ = class.write_packet(chunk).await;
        }
    }
}

#[embassy_executor::task]
async fn sdc_task(sdc: &'static SoftdeviceController<'static>) -> ! {
    let mut evt_buf = [0u8; sdc::raw::HCI_MSG_BUFFER_MAX_SIZE as usize];
    loop {
        match sdc.hci_get(&mut evt_buf).await {
            Ok(bt_hci::PacketKind::Event) => log_hci_event(&evt_buf),
            Ok(bt_hci::PacketKind::AclData) => handle_acl(sdc, &evt_buf),
            Ok(kind) => defmt::debug!("HCI packet kind={:?}", kind),
            Err(e) => defmt::warn!("sdc hci_get error: {:?}", e),
        }
    }
}

fn log_hci_event(buf: &[u8]) {
    match <EventPacket<'_> as bt_hci::FromHciBytes>::from_hci_bytes(buf) {
        Ok((packet, _)) => {
            let event_len = packet.data.len() + 2;
            log_hci_event_packet(packet, event_len);
        }
        Err(_) => defmt::warn!("failed to parse HCI event"),
    }
}

fn log_hci_event_packet(packet: EventPacket<'_>, event_len: usize) {
    match packet.kind.0 {
        code if code == 0x3e && packet.data.len() >= 2 => {
            let subevent = packet.data[0];
            let status = packet.data[1];
            usb_log!("LE event subevent={} status={}", subevent, status);
            if status == 0 && (subevent == 1 || subevent == 10) && packet.data.len() >= 4 {
                let handle = u16::from_le_bytes([packet.data[2], packet.data[3]]) & 0x0fff;
                CONN_HANDLE.store(handle, Ordering::Relaxed);
                defmt::info!("LE connected subevent={} handle={}", subevent, handle);
                usb_log!("LE connected subevent={} handle={}", subevent, handle);
                return;
            }
            defmt::info!("LE event subevent={} status={}", subevent, status);
        }
        0x05 if packet.data.len() >= 4 => {
            let handle = u16::from_le_bytes([packet.data[1], packet.data[2]]) & 0x0fff;
            let reason = packet.data[3];
            CONN_HANDLE.store(0xffff, Ordering::Relaxed);
            defmt::info!("disconnected handle={} reason={}", handle, reason);
            usb_log!("disconnected handle={} reason={}", handle, reason);
        }
        code => {
            defmt::debug!("HCI event code={} len={}", code, event_len);
            usb_log!("HCI event code={} len={}", code, event_len);
        }
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
        0x0004 => {
            usb_log!(
                "ACL ATT len={} opcode={}",
                l2cap_len,
                payload.get(0).copied().unwrap_or(0)
            );
            handle_att(sdc, handle, payload);
        }
        0x0005 => {
            usb_log!(
                "ACL L2CAP len={} code={}",
                l2cap_len,
                payload.get(0).copied().unwrap_or(0)
            );
            handle_l2cap_control(sdc, handle, payload);
        }
        0x0006 => {
            usb_log!(
                "ACL SMP len={} code={}",
                l2cap_len,
                payload.get(0).copied().unwrap_or(0)
            );
            handle_smp(sdc, handle, payload);
        }
        _ => defmt::debug!("ACL cid={} len={}", cid, l2cap_len),
    }
}

fn handle_att(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }

    match pdu[0] {
        // Exchange MTU Request -> 23-byte MTU response.
        0x02 => send_l2cap(sdc, handle, 0x0004, &[0x03, 23, 0]),

        // Find Information Request.
        0x04 => handle_find_information(sdc, handle, pdu),

        // Read By Type Request. Expose a single Device Name characteristic.
        0x08 => handle_read_by_type(sdc, handle, pdu),

        // Read Request for Device Name value handle.
        0x0a if pdu.len() >= 3 && u16::from_le_bytes([pdu[1], pdu[2]]) == 3 => {
            send_l2cap(sdc, handle, 0x0004, b"\x0bESB CONN");
        }

        // Read By Group Type Request. Expose Generic Access primary service.
        0x10 => handle_read_by_group_type(sdc, handle, pdu),

        // Write Request -> Write Response.
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
        send_l2cap(sdc, handle, 0x0004, b"\x09\x0b\x03\x00ESB CONN");
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
        // Connection Parameter Update Request -> accepted.
        0x12 => send_l2cap(sdc, handle, 0x0005, &[0x13, ident, 2, 0, 0, 0]),
        _ => send_l2cap(sdc, handle, 0x0005, &[0x01, ident, 2, 0, code, 0]),
    }
}

fn handle_smp(sdc: &SoftdeviceController<'_>, handle: u16, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }

    // Pairing Failed: Pairing Not Supported.
    if pdu[0] == 0x01 {
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
    let builder = sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .peripheral_count(1)?;
    let required = builder.required_memory()?;
    defmt::info!(
        "SDC connectable required memory={} configured={}",
        required,
        N
    );
    usb_log!("SDC memory required={} configured={}", required, N);
    builder.build(p, rng, mpsl, mem)
}

async fn start_connectable_advertising(sdc: &SoftdeviceController<'_>) {
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
        0x09, 0x09, b'E', b'S', b'B', b' ', b'C', b'O', b'N', b'N', // Complete name.
    ];
    let mut data = [0u8; 31];
    data[..adv_data.len()].copy_from_slice(adv_data);
    LeSetAdvData::new(adv_data.len() as u8, data)
        .exec(sdc)
        .await
        .unwrap();

    LeSetAdvEnable::new(true).exec(sdc).await.unwrap();
    defmt::info!("connectable advertising enabled as ESB CONN");
    usb_log!("connectable advertising enabled as ESB CONN");
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

    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);

    let mut config = embassy_usb::Config::new(0x1209, 0x0002);
    config.manufacturer = Some("embassy-nrf-esb");
    config.product = Some("MPSL BLE connectable");

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static CDC_STATE: StaticCell<State<'static>> = StaticCell::new();

    let mut builder = embassy_usb::Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());
    spawner.spawn(log_task(class).unwrap());

    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );

    static RNG: StaticCell<Rng> = StaticCell::new();
    let rng = RNG.init(rng::Rng::new_blocking(p.RNG));

    static SDC_MEM: StaticCell<sdc::Mem<8192>> = StaticCell::new();
    static SDC: StaticCell<SoftdeviceController> = StaticCell::new();
    let sdc = SDC.init(build_sdc(sdc_p, rng, mpsl, SDC_MEM.init(sdc::Mem::new())).unwrap());

    start_connectable_advertising(sdc).await;
    spawner.spawn(sdc_task(sdc).unwrap());

    loop {
        embassy_time::Timer::after_secs(60).await;
    }
}
