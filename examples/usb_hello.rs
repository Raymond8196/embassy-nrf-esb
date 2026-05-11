#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::usb::Driver as UsbDriver;
use embassy_nrf::{bind_interrupts, peripherals, usb};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::UsbDevice;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        cortex_m::asm::wfe();
    }
}

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

type MyUsbDriver = UsbDriver<'static, HardwareVbusDetect>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyUsbDriver>) {
    device.run().await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let driver = UsbDriver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = embassy_usb::Config::new(0x1209, 0x0001);
    config.manufacturer = Some("Test");
    config.product = Some("USB Hello");
    config.serial_number = Some("00000001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

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

    loop {
        class.wait_connection().await;
        let _ = class.write_packet(b"Hello from nRF52840!\r\n").await;
        loop {
            let _ = class.write_packet(b"tick\r\n").await;
            embassy_time::Timer::after_millis(1000).await;
        }
    }
}
