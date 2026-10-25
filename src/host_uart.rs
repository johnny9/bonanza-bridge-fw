use crate::{
    dma_ring::{DmaRing, Ring},
    runtime,
};
use bonanza_bridge_fw::{
    queue::{uart_word_error, RX_POLL_US, TRANSFER_BYTES},
    transport::{Activity, Code, Fault, Source},
};
use embassy_rp::{
    dma, pac,
    peripherals::{DMA_CH2, DMA_CH3, DMA_CH5, UART1},
    uart::{Blocking, Uart, UartRx, UartTx},
    Peripheral, PeripheralRef,
};
use embassy_time::{Instant, Timer};

static HOST_RING: Ring = Ring::new();
pub struct HostRx {
    _uart: UartRx<'static, UART1, Blocking>,
    ring: DmaRing,
}
pub struct HostTx {
    _uart: UartTx<'static, UART1, Blocking>,
    dma: PeripheralRef<'static, DMA_CH3>,
}
pub struct HostUart {
    pub tx: HostTx,
    pub rx: HostRx,
}

impl HostUart {
    pub fn new(uart: Uart<'static, UART1, Blocking>, rx_dma: DMA_CH2, tx_dma: DMA_CH3, reload: DMA_CH5) -> Self {
        let (tx, rx) = uart.split();
        pac::UART1.uartimsc().write(|w| w.0 = 0);
        pac::UART1.uartdmacr().write(|w| {
            w.set_rxdmae(true);
            w.set_txdmae(true);
            w.set_dmaonerr(true);
        });
        Self {
            tx: HostTx { _uart: tx, dma: tx_dma.into_ref() },
            rx: HostRx {
                _uart: rx,
                ring: DmaRing::new(rx_dma.into(), reload.into(), pac::UART1.uartdr().as_ptr().cast(), pac::dma::vals::TreqSel::UART1_RX, &HOST_RING, 0),
            },
        }
    }
    pub fn start(&mut self) {
        self.rx.ring.start();
    }
    pub async fn reset(&mut self) {
        self.rx.ring.stop().await;
        self.tx.flush().await;
        // Data transmitter on ESP is drained and ASIC reset held by the caller.
        // Discard all prior-generation input and sticky UART errors.
        while !pac::UART1.uartfr().read().rxfe() {
            let _ = pac::UART1.uartdr().read();
        }
        pac::UART1.uartrsr().write(|w| w.0 = 0);
        pac::UART1.uarticr().write(|w| w.0 = 0x7ff);
        self.rx.ring.start();
    }
}
impl HostRx {
    #[cfg(feature = "bench-source")]
    pub fn check_capture(&mut self) -> Result<(), Fault> {
        // The synthetic source replaces the normal host-consumer loop. Keep
        // sampling its DMA counter while generating the return-path load.
        self.ring.read(&mut []).map(|_| ())
    }

    pub async fn read(&mut self, output: &mut [u8]) -> Result<usize, Fault> {
        let mut raw = [0u32; TRANSFER_BYTES];
        loop {
            runtime::service(Activity::Tx);
            let count = self.ring.read(&mut raw[..output.len().min(TRANSFER_BYTES)])?;
            // DMA reads the full UART DR, retaining flags rather than dropping
            // the character and reporting its error after later bytes.
            let error_bits = raw[..count].iter().fold(0, |bits, word| bits | (word & 0xf00));
            if let Some(error) = uart_word_error(error_bits) {
                runtime::queues(|s| {
                    s.uart_error_bits |= error_bits;
                    s.uart_errors = s.uart_errors.wrapping_add(1);
                });
                runtime::update(|s| s.counters.esp_uart_errors = s.counters.esp_uart_errors.wrapping_add(1));
                return Err(Fault::new(Source::EspUart, error));
            }
            if count != 0 {
                for (byte, word) in output.iter_mut().zip(raw[..count].iter()) {
                    *byte = *word as u8;
                }
                return Ok(count);
            }
            Timer::after_micros(RX_POLL_US).await;
        }
    }
}
impl HostTx {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        let started = Instant::now().as_micros();
        unsafe {
            dma::write(self.dma.reborrow(), bytes, pac::UART1.uartdr().as_ptr().cast::<u8>(), pac::dma::vals::TreqSel::UART1_TX).await;
        }
        let elapsed = Instant::now().as_micros() - started;
        runtime::queues(|s| {
            s.tx_blocks[1] = s.tx_blocks[1].wrapping_add(1);
            s.tx_max_us[1] = s.tx_max_us[1].max(elapsed as u32);
        });
        let ctrl = pac::DMA.ch(3).ctrl_trig().read();
        if ctrl.ahb_error() {
            return Err(Fault::new(Source::Dma, Code::DmaBus));
        }
        Ok(())
    }
    pub async fn flush(&mut self) {
        while pac::UART1.uartfr().read().busy() {
            Timer::after_micros(10).await;
        }
    }
}
