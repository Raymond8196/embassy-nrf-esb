//! Continuous-loop PTX for 3-mode coexistence testing.
//!
//! Starts ESB transmissions immediately on boot. Alternates between pipe 0
//! and pipe 1 in repeated batches. USB CDC is optional: if a host connects,
//! statistics are printed; otherwise the radio keeps running.
//!
//! Designed to pair with `mpsl_3mode_central`.

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;

use embassy_executor::Spawner;
use embassy_nrf::interrupt::typelevel;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use nrf_mpsl::{MultiprotocolServiceLayer, Peripherals, SessionMem, raw};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use embassy_nrf_esb::addresses::EsbAddresses;
use embassy_nrf_esb::config::EsbConfig;
use embassy_nrf_esb::mpsl_timeslot::run_ptx_slots;

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

bind_interrupts!(struct Irqs {
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
async fn hfclk_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    let _hfclk = mpsl.request_hfclk().await.unwrap();
    core::future::pending().await
}

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver<'static, &'static SoftwareVbusDetect>>) {
    device.run().await
}

const BATCH_SLOTS: u32 = 20;
const SLOT_LENGTH_US: u32 = 14000;
const IN_SLOT_MATCH_US: u32 = 13500;
const PACKETS_PER_SLOT: u32 = 10;

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

    let mpsl_p =
        Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);

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

    // USB CDC (optional output, doesn't block ESB)
    static VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
    let vbus: &'static SoftwareVbusDetect = VBUS.init(SoftwareVbusDetect::new(true, true));
    let driver = UsbDriver::new(p.USBD, Irqs, vbus);

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("ESB 3Mode");
    config.product = Some("PTX Continuous");

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

    let mut cdc_class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    let usb_dev = builder.build();
    spawner.spawn(usb_task(usb_dev).unwrap());

    let esb_cfg = EsbConfig::default();
    let esb_addr = EsbAddresses::new(
        [0xE7, 0xE7, 0xE7, 0xE7],
        [0xC2, 0xC2, 0xC2, 0xC2],
        [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
        8,
    )
    .unwrap();

    defmt::info!("PTX continuous starting");

    let mut pipe = 0u8;
    let mut round: u32 = 0;
    let mut total_tx: u32 = 0;
    let mut total_ack: u32 = 0;
    let mut cdc_connected = false;

    let mut err_count: u32 = 0;

    loop {
        round += 1;

        let r = match run_ptx_slots(
            mpsl,
            &esb_cfg,
            &esb_addr,
            SLOT_LENGTH_US,
            IN_SLOT_MATCH_US,
            BATCH_SLOTS,
            pipe,
            PACKETS_PER_SLOT,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                err_count += 1;
                defmt::warn!("run_ptx_slots error {:?} (count={})", e, err_count);
                embassy_time::Timer::after_millis(100).await;
                continue;
            }
        };

        total_tx += r.tx_count;
        total_ack += r.ack_ok_count;

        defmt::info!(
            "pipe={} tx={} ack={} ackpl={} round={}",
            pipe,
            r.tx_count,
            r.ack_ok_count,
            r.ack_payload_count,
            round,
        );

        // Try to log to CDC if connected
        if !cdc_connected {
            // Non-blocking attempt: just try to write a byte to see if connected
            // Actually embassy-usb CdcAcmClass doesn't have a non-blocking check.
            // We just try the write and ignore timeout.
            cdc_connected = true; // Assume connected after first round; writes will just be dropped if not
        }

        if cdc_connected {
            let mut buf = [0u8; 256];
            let mut w = WriteBuf::new(&mut buf);
            let _ = write!(
                w,
                "pipe={} tx={} ack={} ackpl={} round={} tot_tx={} tot_ack={}\r\n",
                pipe,
                r.tx_count,
                r.ack_ok_count,
                r.ack_payload_count,
                round,
                total_tx,
                total_ack,
            );
            let pos = w.pos;
            for chunk in buf[..pos].chunks(64) {
                let _ = cdc_class.write_packet(chunk).await;
            }

            if round % 10 == 0 {
                let mut buf2 = [0u8; 128];
                let mut w2 = WriteBuf::new(&mut buf2);
                let _ = write!(
                    w2,
                    "[SUM] tot_tx={} tot_ack={} ack_rate={}%\r\n",
                    total_tx,
                    total_ack,
                    if total_tx > 0 {
                        total_ack * 100 / total_tx
                    } else {
                        0
                    },
                );
                let pos2 = w2.pos;
                for chunk in buf2[..pos2].chunks(64) {
                    let _ = cdc_class.write_packet(chunk).await;
                }
            }
        }

        pipe = if pipe == 0 { 1 } else { 0 };
    }
}
