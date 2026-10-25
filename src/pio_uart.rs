use crate::{
    dma_ring::{DmaRing, Ring},
    runtime,
};
use bonanza_bridge_fw::{
    pio_programs,
    queue::{RX_POLL_US, TRANSFER_BYTES},
    transport::{Activity, Code, Fault, Source},
    uart_codec::asic_rx_word_to_u8,
    uart_timing::clock_divider_bits,
};
use embassy_rp::{
    clocks::clk_sys_freq,
    gpio::{Level, Pull},
    pac,
    peripherals::{DMA_CH0, DMA_CH1, DMA_CH4, PIO1},
    pio::{Common, Config, Direction, FifoJoin, PioPin, ShiftDirection, StateMachine},
    Peripheral, PeripheralRef,
};
use embassy_time::{Instant, Timer};

const RX_SM: usize = 1;
const FRAMING_IRQ: u8 = 1 << 4;

static ASIC_RING: Ring = Ring::new();

pub struct AsicTx {
    sm: StateMachine<'static, PIO1, 0>,
    dma: PeripheralRef<'static, DMA_CH1>,
    origin: u8,
}

pub struct AsicRx {
    sm: StateMachine<'static, PIO1, 1>,
    ring: DmaRing,
    origin: u8,
}

pub fn new(common: &mut Common<'static, PIO1>, mut tx: StateMachine<'static, PIO1, 0>, mut rx: StateMachine<'static, PIO1, 1>, tx_pin: impl PioPin, rx_pin: impl PioPin, channels: (DMA_CH0, DMA_CH1, DMA_CH4), baud: u32) -> (AsicTx, AsicRx) {
    let (dma, tx_dma, reload) = channels;
    let tx_pin = common.make_pio_pin(tx_pin);
    tx.set_pins(Level::High, &[&tx_pin]);
    tx.set_pin_dirs(Direction::Out, &[&tx_pin]);
    let tx_program = common.load_program(&pio_programs::tx());
    let divider = fixed::FixedU32::from_bits(clock_divider_bits(clk_sys_freq(), baud).unwrap());
    let mut config = Config::default();
    config.use_program(&tx_program, &[&tx_pin]);
    config.set_out_pins(&[&tx_pin]);
    config.set_set_pins(&[&tx_pin]);
    config.shift_out.direction = ShiftDirection::Right;
    config.shift_out.auto_fill = false;
    config.fifo_join = FifoJoin::TxOnly;
    config.clock_divider = divider;
    tx.set_config(&config);
    tx.set_enable(false);

    let mut rx_pin = common.make_pio_pin(rx_pin);
    rx_pin.set_pull(Pull::Up);
    rx.set_pin_dirs(Direction::In, &[&rx_pin]);
    let rx_program = common.load_program(&pio_programs::rx());
    let mut config = Config::default();
    config.use_program(&rx_program, &[]);
    config.set_in_pins(&[&rx_pin]);
    config.set_jmp_pin(&rx_pin);
    #[cfg(feature = "bench-loopback")]
    {
        config.set_in_pins(&[&tx_pin]);
        config.set_jmp_pin(&tx_pin);
    }
    config.shift_in.direction = ShiftDirection::Right;
    // PUSH is explicit and only occurs after a valid stop bit.
    config.shift_in.auto_fill = false;
    config.shift_in.threshold = 32;
    config.fifo_join = FifoJoin::RxOnly;
    config.clock_divider = divider;
    rx.set_config(&config);
    rx.set_enable(false);

    (
        AsicTx {
            sm: tx,
            dma: tx_dma.into_ref(),
            origin: tx_program.origin,
        },
        AsicRx {
            sm: rx,
            ring: DmaRing::new(dma.into(), reload.into(), pac::PIO1.rxf(RX_SM).as_ptr(), pac::dma::vals::TreqSel::PIO1_RX1, &ASIC_RING, 1),
            origin: rx_program.origin,
        },
    )
}

impl AsicTx {
    pub async fn write(&mut self, words: &[u16]) -> Result<(), Fault> {
        let started = Instant::now().as_micros();
        self.sm.tx().dma_push(self.dma.reborrow(), words).await;
        let elapsed = Instant::now().as_micros() - started;
        runtime::queues(|s| {
            s.tx_blocks[0] = s.tx_blocks[0].wrapping_add(1);
            s.tx_max_us[0] = s.tx_max_us[0].max(elapsed as u32);
        });
        if pac::DMA.ch(1).ctrl_trig().read().ahb_error() {
            return Err(Fault::new(Source::Dma, Code::DmaBus));
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.sm.set_enable(false);
        self.sm.clear_fifos();
        self.sm.restart();
        // With the SM stopped and SET mapped to TX, force idle high and
        // discard the OSR/PC's partial word before restarting at PULL.
        unsafe {
            self.sm.exec_instr(0xe001);
            self.sm.exec_instr(self.origin as u16);
        }
    }

    pub fn start(&mut self) {
        self.sm.clkdiv_restart();
        self.sm.set_enable(true);
    }
}

impl AsicRx {
    pub fn start(&mut self) {
        self.sm.clear_fifos();
        self.sm.restart();
        self.sm.clkdiv_restart();
        // Enter through WAIT idle, not through the normal start-bit wrap.
        unsafe {
            self.sm.exec_instr(self.origin as u16);
        }
        self.ring.start();
        self.sm.set_enable(true);
    }

    pub async fn stop(&mut self) {
        self.sm.set_enable(false);
        self.ring.stop().await;
        let _ = self.observe_errors();
        self.sm.clear_fifos();
        self.sm.restart();
    }

    fn observe_errors(&mut self) -> Option<Fault> {
        let stalled = pac::PIO1.fdebug().read().rxstall() & (1 << RX_SM) != 0;
        if stalled {
            pac::PIO1.fdebug().write(|w| w.set_rxstall(1 << RX_SM));
        }
        let framing = pac::PIO1.irq().read().irq() & FRAMING_IRQ != 0;
        if framing {
            pac::PIO1.irq().write(|w| w.set_irq(FRAMING_IRQ));
        }
        if stalled || framing {
            runtime::update(|s| {
                if stalled {
                    s.counters.pio_stall_events = s.counters.pio_stall_events.wrapping_add(1);
                    s.record(Fault::new(Source::AsicRx, Code::FifoStall));
                }
                if framing {
                    s.counters.pio_framing_events = s.counters.pio_framing_events.wrapping_add(1);
                    s.record(Fault::new(Source::AsicRx, Code::Framing));
                }
            });
        }
        // A bad stop can occur while ASIC power/reset changes. FIFO exhaustion
        // means capture lost data and must stop the stream instead of forwarding
        // an incomplete response as though the receive queue were intact.
        stalled.then(|| Fault::new(Source::AsicRx, Code::FifoStall))
    }

    pub async fn receive(&mut self, output: &mut [u8]) -> Result<usize, Fault> {
        let mut raw = [0u32; TRANSFER_BYTES];
        loop {
            runtime::service(Activity::Rx);
            if let Some(fault) = self.observe_errors() {
                return Err(fault);
            }
            let count = match self.ring.read(&mut raw[..output.len().min(TRANSFER_BYTES)]) {
                Ok(count) => count,
                Err(fault) => return Err(fault),
            };
            if count != 0 {
                for (byte, word) in output.iter_mut().zip(raw[..count].iter()) {
                    *byte = asic_rx_word_to_u8(*word);
                }
                runtime::update(|s| s.counters.asic_rx_bytes = s.counters.asic_rx_bytes.wrapping_add(count as u32));
                return Ok(count);
            }
            Timer::after_micros(RX_POLL_US).await;
        }
    }
}
