use crate::runtime;
use bonanza_bridge_fw::{
    queue::{Cursor, DMA_RELOAD_COUNT, MAX_PROGRESS_GAP_US, RX_RING_BYTES, RX_RING_WORDS},
    transport::{Code, Fault, Source},
};
use core::{
    cell::UnsafeCell,
    sync::atomic::{compiler_fence, Ordering},
};
use embassy_rp::{
    dma::{AnyChannel, Channel},
    pac,
};
use embassy_time::{Instant, Timer};

#[repr(C, align(16384))]
pub struct Ring(UnsafeCell<[u32; RX_RING_WORDS]>);
const _: () = assert!(RX_RING_BYTES == core::mem::align_of::<Ring>());
// DMA is the sole producer. A unique DmaRing consumer reads volatile words and
// rejects a copied window if DMA overtook it. No CPU reference aliases storage.
unsafe impl Sync for Ring {}
impl Ring {
    pub const fn new() -> Self {
        Self(UnsafeCell::new([0; RX_RING_WORDS]))
    }
}
static RELOAD: u32 = DMA_RELOAD_COUNT;

pub struct DmaRing {
    data: AnyChannel,
    reload: AnyChannel,
    source: *const u32,
    dreq: pac::dma::vals::TreqSel,
    ring: &'static Ring,
    cursor: Cursor,
    diagnostic: usize,
    last_poll: Option<u64>,
}

impl DmaRing {
    pub fn new(data: AnyChannel, reload: AnyChannel, source: *const u32, dreq: pac::dma::vals::TreqSel, ring: &'static Ring, diagnostic: usize) -> Self {
        Self {
            data,
            reload,
            source,
            dreq,
            ring,
            cursor: Cursor::new(),
            diagnostic,
            last_poll: None,
        }
    }
    pub fn start(&mut self) {
        let data = self.data.regs();
        let reload = self.reload.regs();
        let mask: u32 = (1 << self.data.number()) | (1 << self.reload.number());
        pac::DMA.inte(0).modify(|w| *w &= !mask);
        pac::DMA.inte(1).modify(|w| *w &= !mask);
        pac::DMA.intr(0).write_value(mask);
        // The reload channel writes the count trigger, not the address. Hardware
        // ring wrapping keeps the producer continuous without waiting for a task.
        reload.read_addr().write_value(core::ptr::addr_of!(RELOAD) as u32);
        reload.write_addr().write_value(data.al1_trans_count_trig().as_ptr() as u32);
        reload.trans_count().write_value(1);
        reload.al1_ctrl().write_value({
            let mut w = pac::dma::regs::CtrlTrig(0);
            w.set_read_error(true);
            w.set_write_error(true);
            w.set_data_size(pac::dma::vals::DataSize::SIZE_WORD);
            w.set_chain_to(self.reload.number());
            w.set_treq_sel(pac::dma::vals::TreqSel::PERMANENT);
            w.set_irq_quiet(true);
            w.set_en(true);
            w.0
        });
        data.read_addr().write_value(self.source as u32);
        data.write_addr().write_value(self.ring.0.get().cast::<u32>() as u32);
        data.trans_count().write_value(DMA_RELOAD_COUNT);
        self.cursor = Cursor::new();
        self.last_poll = None;
        compiler_fence(Ordering::SeqCst);
        data.ctrl_trig().write(|w| {
            w.set_read_error(true);
            w.set_write_error(true);
            w.set_high_priority(true);
            w.set_data_size(pac::dma::vals::DataSize::SIZE_WORD);
            w.set_incr_write(true);
            w.set_ring_size(RX_RING_BYTES.ilog2() as u8);
            w.set_ring_sel(true);
            w.set_chain_to(self.reload.number());
            w.set_treq_sel(self.dreq);
            w.set_irq_quiet(true);
            w.set_en(true);
        });
    }
    pub async fn stop(&mut self) {
        let mask: u32 = (1 << self.data.number()) | (1 << self.reload.number());
        // Disable both before aborting, so a late chain trigger cannot restart RX.
        self.reload.regs().ctrl_trig().modify(|w| w.set_en(false));
        self.data.regs().ctrl_trig().modify(|w| w.set_en(false));
        pac::DMA.chan_abort().write(|w| w.set_chan_abort(mask as u16));
        while pac::DMA.chan_abort().read().chan_abort() as u32 & mask != 0 || self.data.regs().ctrl_trig().read().busy() || self.reload.regs().ctrl_trig().read().busy() {
            Timer::after_micros(10).await;
        }
        compiler_fence(Ordering::SeqCst);
        pac::DMA.intr(0).write_value(mask);
        self.cursor = Cursor::new();
        self.last_poll = None;
    }
    fn observe(&mut self) -> Result<(), Fault> {
        for channel in [&self.data, &self.reload] {
            let ctrl = channel.regs().ctrl_trig().read();
            if ctrl.read_error() || ctrl.write_error() || !ctrl.en() {
                return Err(Fault::new(Source::Dma, Code::DmaBus));
            }
        }
        let before = self.cursor.reloads;
        self.cursor.observe(self.data.regs().trans_count().read()).map_err(|e| Fault::new(Source::Dma, e))?;
        if before != self.cursor.reloads {
            runtime::queues(|s| s.rx_reloads[self.diagnostic] = s.rx_reloads[self.diagnostic].wrapping_add(1));
        }
        Ok(())
    }
    pub fn read(&mut self, output: &mut [u32]) -> Result<usize, Fault> {
        let now = Instant::now().as_micros();
        let gap = self.last_poll.replace(now).map_or(0, |last| now.saturating_sub(last).min(u32::MAX as u64) as u32);
        if gap as u64 >= MAX_PROGRESS_GAP_US {
            return Err(Fault::new(Source::Dma, Code::RingOverwrite));
        }
        self.observe()?;
        let queued = (self.cursor.produced - self.cursor.consumed).min(u32::MAX as u64) as u32;
        runtime::queues(|s| {
            s.rx_highwater[self.diagnostic] = s.rx_highwater[self.diagnostic].max(queued);
            s.rx_max_gap_us[self.diagnostic] = s.rx_max_gap_us[self.diagnostic].max(gap);
        });
        let result = self.copy(output);
        if result == Err(Fault::new(Source::Dma, Code::RingOverwrite)) {
            runtime::queues(|s| s.rx_overwrites[self.diagnostic] = s.rx_overwrites[self.diagnostic].wrapping_add(1));
            if self.diagnostic == 1 {
                let lost = (self.cursor.produced - self.cursor.consumed).saturating_sub(RX_RING_WORDS as u64) as u32;
                runtime::update(|s| s.counters.ring_dropped_words = s.counters.ring_dropped_words.wrapping_add(lost));
            }
        }
        result
    }
    fn copy(&mut self, output: &mut [u32]) -> Result<usize, Fault> {
        let count = self.cursor.available().map_err(|e| Fault::new(Source::Dma, e))?.min(output.len());
        let first = self.cursor.consumed;
        compiler_fence(Ordering::Acquire);
        for (offset, word) in output[..count].iter_mut().enumerate() {
            let index = (first as usize + offset) % RX_RING_WORDS;
            *word = unsafe { core::ptr::read_volatile(self.ring.0.get().cast::<u32>().add(index)) };
        }
        compiler_fence(Ordering::SeqCst);
        self.observe()?;
        self.cursor.commit_copy(first, count).map_err(|e| Fault::new(Source::Dma, e))?;
        Ok(count)
    }
}
