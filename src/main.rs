#![no_std]
#![no_main]

use defmt::unwrap;
use defmt_rtt as _;
use panic_probe as _;

use embassy_executor::Spawner;
use embassy_rp::interrupt::{InterruptExt, Priority};
use embassy_rp::{
    bind_interrupts, interrupt,
    peripherals::{PIO1, UART0, UART1},
    pio::{self},
    uart as rp_uart,
};
use embassy_time::Duration;
use static_cell::StaticCell;

use bonanza_bridge_fw::image_manifest;
use bonanza_bridge_fw::transport::WATCHDOG_TIMEOUT_MS;
use bonanza_bridge_fw::uart_timing::ESP_DATA_BAUD_RATE;

mod control;
mod pio_uart;
mod runtime;
mod uart;

const CONTROL_BAUDRATE: u32 = 115_200;
const ASIC_DATA_BAUDRATE: u32 = 5_000_000;

#[repr(C, align(4))]
struct AlignedImageManifest([u8; image_manifest::MANIFEST_SIZE]);

#[used]
#[link_section = ".rodata.bzm_bridge_manifest"]
static BRIDGE_IMAGE_MANIFEST: AlignedImageManifest = AlignedImageManifest(image_manifest::IMAGE_MANIFEST);

bind_interrupts!(struct Irqs {
    UART0_IRQ => rp_uart::BufferedInterruptHandler<UART0>;
    UART1_IRQ => rp_uart::BufferedInterruptHandler<UART1>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    core::hint::black_box(&BRIDGE_IMAGE_MANIFEST);
    let p = embassy_rp::init(Default::default());

    // PIO IRQ remains highest priority for command TX futures. ASIC RX uses a
    // paced DMA channel and therefore does not depend on executor IRQ latency.
    interrupt::PIO1_IRQ_0.set_priority(Priority::P0);
    interrupt::UART1_IRQ.set_priority(Priority::P1);
    interrupt::UART0_IRQ.set_priority(Priority::P2);

    let mut watchdog = embassy_rp::watchdog::Watchdog::new(p.WATCHDOG);
    runtime::update(|s| s.watchdog_reason = embassy_rp::pac::WATCHDOG.reason().read().0 as u8);
    watchdog.start(Duration::from_millis(WATCHDOG_TIMEOUT_MS));

    let control_uart = {
        static TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
        static RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();

        let mut config = rp_uart::Config::default();
        config.baudrate = CONTROL_BAUDRATE;

        rp_uart::BufferedUart::new(p.UART0, Irqs, p.PIN_0, p.PIN_1, TX_BUF.init([0; 256]), RX_BUF.init([0; 256]), config)
    };

    let data_uart = {
        static TX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
        static RX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();

        let mut config = rp_uart::Config::default();
        config.baudrate = ESP_DATA_BAUD_RATE;

        rp_uart::BufferedUart::new(p.UART1, Irqs, p.PIN_4, p.PIN_5, TX_BUF.init([0; 4096]), RX_BUF.init([0; 4096]), config)
    };

    let pio::Pio { mut common, sm0, sm1, .. } = pio::Pio::new(p.PIO1, Irqs);
    let (asic_tx, asic_rx) = pio_uart::new(&mut common, sm0, sm1, p.PIN_8, p.PIN_9, p.DMA_CH0, ASIC_DATA_BAUDRATE);
    unwrap!(spawner.spawn(control::uart_task(control_uart)));
    unwrap!(spawner.spawn(uart::transport_task(data_uart, asic_tx, asic_rx)));
    unwrap!(spawner.spawn(runtime::watchdog_task(watchdog)));

    core::future::pending::<()>().await;
}
