use crate::runtime;
use bonanza_bridge_fw::{
    pio_programs,
    transport::{Activity, Code, Fault, Source},
    uart_codec::asic_rx_word_to_u8,
    uart_timing::{clock_divider_bits, dma_copy_is_valid, dma_ring_window, ASIC_RX_DMA_RING_WORDS, ASIC_RX_DMA_TRANSFER_COUNT},
};
use core::{
    cell::UnsafeCell,
    sync::atomic::{compiler_fence, Ordering},
};
use embassy_rp::{
    clocks::clk_sys_freq,
    gpio::{Level, Pull},
    pac,
    peripherals::{DMA_CH0, PIO1},
    pio::{Common, Config, Direction, FifoJoin, PioPin, ShiftDirection, StateMachine},
};
use embassy_time::Timer;

const RX_SM: usize = 1;
const DMA_CHANNEL: usize = 0;
const FRAMING_IRQ: u8 = 1 << 4;

#[repr(C, align(4096))]
struct DmaRing(UnsafeCell<[u32; ASIC_RX_DMA_RING_WORDS]>);
// Only AsicRx (which owns DMA_CH0) accesses this ring. DMA writes concurrently;
// the sole CPU reader uses volatile reads and validates the copied window.
unsafe impl Sync for DmaRing {}
static RING: DmaRing = DmaRing(UnsafeCell::new([0; ASIC_RX_DMA_RING_WORDS]));

pub struct AsicTx {
    sm: StateMachine<'static, PIO1, 0>,
    origin: u8,
}

pub struct AsicRx {
    sm: StateMachine<'static, PIO1, 1>,
    _dma: DMA_CH0,
    origin: u8,
    consumed: u32,
}

pub fn new(common: &mut Common<'static, PIO1>, mut tx: StateMachine<'static, PIO1, 0>, mut rx: StateMachine<'static, PIO1, 1>, tx_pin: impl PioPin, rx_pin: impl PioPin, dma: DMA_CH0, baud: u32) -> (AsicTx, AsicRx) {
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
    config.shift_in.direction = ShiftDirection::Right;
    // PUSH is explicit and only occurs after a valid stop bit.
    config.shift_in.auto_fill = false;
    config.shift_in.threshold = 32;
    config.fifo_join = FifoJoin::RxOnly;
    config.clock_divider = divider;
    rx.set_config(&config);
    rx.set_enable(false);

    (
        AsicTx { sm: tx, origin: tx_program.origin },
        AsicRx {
            sm: rx,
            _dma: dma,
            origin: rx_program.origin,
            consumed: 0,
        },
    )
}

impl AsicTx {
    pub async fn write(&mut self, word: u16) {
        self.sm.tx().wait_push((word & 0x1ff) as u32).await;
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
    fn start_dma(&mut self) {
        let channel = pac::DMA.ch(DMA_CHANNEL);
        channel.read_addr().write_value(pac::PIO1.rxf(RX_SM).as_ptr() as u32);
        channel.write_addr().write_value(RING.0.get().cast::<u32>() as u32);
        channel.trans_count().write(|w| *w = ASIC_RX_DMA_TRANSFER_COUNT);
        self.consumed = 0;
        compiler_fence(Ordering::SeqCst);
        channel.ctrl_trig().write(|w| {
            w.set_read_error(true); // W1C sticky errors from an earlier attempt
            w.set_write_error(true);
            w.set_high_priority(true);
            w.set_data_size(pac::dma::vals::DataSize::SIZE_WORD);
            w.set_incr_read(false);
            w.set_incr_write(true);
            w.set_ring_size(12);
            w.set_ring_sel(true);
            w.set_chain_to(DMA_CHANNEL as u8);
            w.set_treq_sel(pac::dma::vals::TreqSel::PIO1_RX1);
            w.set_en(true);
        });
        compiler_fence(Ordering::SeqCst);
    }

    pub fn start(&mut self) {
        self.sm.clear_fifos();
        self.sm.restart();
        self.sm.clkdiv_restart();
        // Enter through WAIT idle, not through the normal start-bit wrap.
        unsafe {
            self.sm.exec_instr(self.origin as u16);
        }
        self.start_dma();
        self.sm.set_enable(true);
    }

    pub async fn stop(&mut self) {
        self.sm.set_enable(false);
        pac::DMA.chan_abort().write(|w| w.set_chan_abort(1 << DMA_CHANNEL));
        // Wait for the abort handshake as well as BUSY. This loop is always
        // enclosed by the transport owner's reset timeout; never spin here.
        while pac::DMA.chan_abort().read().chan_abort() & (1 << DMA_CHANNEL) != 0 || pac::DMA.ch(DMA_CHANNEL).ctrl_trig().read().busy() {
            Timer::after_micros(10).await;
        }
        compiler_fence(Ordering::SeqCst);
        self.observe_errors();
        self.sm.clear_fifos();
        self.sm.restart();
        self.consumed = 0;
    }

    fn observe_errors(&mut self) {
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
    }

    fn record_loss(count: u32) {
        if count != 0 {
            runtime::update(|s| {
                s.counters.ring_dropped_words = s.counters.ring_dropped_words.wrapping_add(count);
                s.record(Fault::new(Source::Dma, Code::RingOverwrite));
            });
        }
    }

    pub async fn receive(&mut self, output: &mut [u8]) -> Result<usize, Fault> {
        assert!(!output.is_empty());
        loop {
            runtime::service(Activity::Rx);
            self.observe_errors();
            let channel = pac::DMA.ch(DMA_CHANNEL);
            let ctrl = channel.ctrl_trig().read();
            if ctrl.read_error() || ctrl.write_error() {
                return Err(Fault::new(Source::Dma, Code::DmaBus));
            }
            let remaining = channel.trans_count().read();
            if remaining > ASIC_RX_DMA_TRANSFER_COUNT || (!ctrl.busy() && remaining != 0) {
                return Err(Fault::new(Source::Dma, Code::DmaBus));
            }
            let produced = ASIC_RX_DMA_TRANSFER_COUNT - remaining;
            let (first, available, dropped) = dma_ring_window(produced, self.consumed, ASIC_RX_DMA_RING_WORDS);
            Self::record_loss(dropped);
            self.consumed = first;
            if available != 0 {
                let count = available.min(output.len());
                compiler_fence(Ordering::Acquire);
                for (offset, byte) in output[..count].iter_mut().enumerate() {
                    let index = first.wrapping_add(offset as u32) as usize % ASIC_RX_DMA_RING_WORDS;
                    let raw = unsafe { core::ptr::read_volatile(RING.0.get().cast::<u32>().add(index)) };
                    *byte = asic_rx_word_to_u8(raw);
                }
                compiler_fence(Ordering::SeqCst);
                let after = ASIC_RX_DMA_TRANSFER_COUNT - channel.trans_count().read();
                if dma_copy_is_valid(after, first, ASIC_RX_DMA_RING_WORDS) {
                    self.consumed = first.wrapping_add(count as u32);
                    runtime::update(|s| s.counters.asic_rx_bytes = s.counters.asic_rx_bytes.wrapping_add(count as u32));
                    return Ok(count);
                }
                // DMA lapped the copy. Do not emit a mixture of old/new words.
                // The next iteration accounts for all lost words exactly once.
            } else if remaining == 0 && !channel.ctrl_trig().read().busy() {
                // Preserve and drain the final ring before rearming the finite
                // counter. The PIO FIFO remains intact during this operation.
                self.start_dma();
                runtime::update(|s| s.counters.dma_rearms = s.counters.dma_rearms.wrapping_add(1));
            }
            Timer::after_micros(250).await;
        }
    }
}
