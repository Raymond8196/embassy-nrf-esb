#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver<'static, HardwareVbusDetect>>) {
    device.run().await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    // USB requires external HFCLK
    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("ESB Test");
    config.product = Some("Minimal USB");

    static CONFIG_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static BOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static MSOS_DESC: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
    static CONTROL_BUF: static_cell::StaticCell<[u8; 64]> = static_cell::StaticCell::new();
    static CDC_STATE: static_cell::StaticCell<State<'static>> = static_cell::StaticCell::new();

    let mut builder = embassy_usb::Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let mut class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    let usb = builder.build();
    spawner.spawn(usb_task(usb).unwrap());

    class.wait_connection().await;
    loop {
        class.write_packet(b"hello\r\n").await.ok();
        embassy_time::Timer::after_secs(1).await;
    }
}
