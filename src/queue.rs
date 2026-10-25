//! Progress accounting shared by both continuously captured receive streams.
use crate::transport::Code;

pub const RX_RING_WORDS: usize = 4096;
pub const RX_RING_BYTES: usize = RX_RING_WORDS * 4;
pub const DMA_RELOAD_COUNT: u32 = 0x0010_0000;
pub const TRANSFER_BYTES: usize = 256;
pub const RX_POLL_US: u64 = 50;
const _: () = assert!((DMA_RELOAD_COUNT as usize).is_multiple_of(RX_RING_WORDS));
const _: () = assert!(TRANSFER_BYTES.is_multiple_of(2));
// Even the faster host stream cannot consume one reload period sooner.
pub const MAX_PROGRESS_GAP_US: u64 = DMA_RELOAD_COUNT as u64 * crate::uart_timing::ESP_TX_WIRE_BITS_PER_BYTE as u64 * 1_000_000 / crate::uart_timing::ESP_DATA_BAUD_RATE as u64;

#[derive(Debug)]
pub struct Cursor {
    pub produced: u64,
    pub consumed: u64,
    pub reloads: u32,
    remaining: u32,
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

impl Cursor {
    pub const fn new() -> Self {
        Self {
            produced: 0,
            consumed: 0,
            reloads: 0,
            remaining: DMA_RELOAD_COUNT,
        }
    }

    pub fn observe(&mut self, remaining: u32) -> Result<(), Code> {
        if remaining > DMA_RELOAD_COUNT {
            return Err(Code::DmaBus);
        }
        let delta = if remaining <= self.remaining {
            self.remaining - remaining
        } else {
            self.reloads = self.reloads.wrapping_add(1);
            self.remaining + DMA_RELOAD_COUNT - remaining
        };
        self.produced += delta as u64;
        self.remaining = remaining;
        Ok(())
    }

    pub fn available(&self) -> Result<usize, Code> {
        let count = self.produced - self.consumed;
        if count > RX_RING_WORDS as u64 {
            Err(Code::RingOverwrite)
        } else {
            Ok(count as usize)
        }
    }

    pub fn commit_copy(&mut self, first: u64, count: usize) -> Result<(), Code> {
        if first != self.consumed || self.produced - first > RX_RING_WORDS as u64 {
            return Err(Code::RingOverwrite);
        }
        self.consumed += count as u64;
        Ok(())
    }
}

/// Keep UART error flags alongside the byte until the consumer checks them.
/// Dropping an errored character before reporting it can shift every byte pair.
pub const fn uart_word_error(word: u32) -> Option<Code> {
    if word & (1 << 11) != 0 {
        Some(Code::Overrun)
    } else if word & (1 << 10) != 0 {
        Some(Code::Break)
    } else if word & (1 << 9) != 0 {
        Some(Code::Parity)
    } else if word & (1 << 8) != 0 {
        Some(Code::Framing)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uart_codec::NineBitPairDecoder;

    #[test]
    fn reload_without_a_software_gap_preserves_position_and_pending_data() {
        for observe_zero in [false, true] {
            let mut c = Cursor::new();
            c.observe(3).unwrap();
            c.consumed = c.produced - 17;
            if observe_zero {
                c.observe(0).unwrap();
            }
            c.observe(DMA_RELOAD_COUNT - 11).unwrap();
            assert_eq!(c.available(), Ok(31));
            assert_eq!(c.produced, DMA_RELOAD_COUNT as u64 + 11);
            assert_eq!(c.reloads, 1);
            assert_eq!(c.produced as usize % RX_RING_WORDS, 11);
        }
    }

    #[test]
    fn full_ring_is_valid_but_overwrite_before_or_during_copy_faults() {
        let mut c = Cursor::new();
        c.observe(DMA_RELOAD_COUNT - RX_RING_WORDS as u32).unwrap();
        assert_eq!(c.available(), Ok(RX_RING_WORDS));
        c.observe(DMA_RELOAD_COUNT - RX_RING_WORDS as u32 - 1).unwrap();
        assert_eq!(c.available(), Err(Code::RingOverwrite));
        assert_eq!(c.commit_copy(0, TRANSFER_BYTES), Err(Code::RingOverwrite));
        assert_eq!(c.consumed, 0);
    }

    #[test]
    fn errored_uart_byte_is_detected_before_it_can_shift_pair_alignment() {
        let raw = [0x55, 1, 0x80 | (1 << 11), 0, 0x90, 1];
        assert_eq!(raw.iter().find_map(|word| uart_word_error(*word)), Some(Code::Overrun));
        for (bit, error) in [(8, Code::Framing), (9, Code::Parity), (10, Code::Break), (11, Code::Overrun)] {
            assert_eq!(uart_word_error(0xff | (1 << bit)), Some(error));
        }
        for byte in 0..256 {
            assert_eq!(uart_word_error(byte), None);
        }
    }

    #[test]
    fn full_rate_bursts_across_ring_wraps_keep_every_word_in_order() {
        let mut ring = [0u8; RX_RING_WORDS];
        let mut c = Cursor::new();
        let mut decoder = NineBitPairDecoder::new();
        let mut sent = 0;
        let mut received = 0;
        for step in 0..80000 {
            // 50 us at 5 Mbps = 25 bytes; also exercise 500 us scheduling gaps.
            let incoming = if step % 101 == 0 { 250 } else { 25 };
            for _ in 0..incoming {
                let word = (sent / 2) as u16 & 0x1ff;
                ring[sent % RX_RING_WORDS] = if sent % 2 == 0 { word as u8 } else { (word >> 8) as u8 };
                sent += 1;
            }
            c.observe(DMA_RELOAD_COUNT - (sent as u32 % DMA_RELOAD_COUNT)).unwrap();
            while c.available().unwrap() != 0 {
                let count = c.available().unwrap().min(TRANSFER_BYTES);
                let mut bytes = [0; TRANSFER_BYTES];
                for (i, byte) in bytes[..count].iter_mut().enumerate() {
                    *byte = ring[(c.consumed as usize + i) % RX_RING_WORDS];
                }
                let mut words = [0; TRANSFER_BYTES / 2];
                let d = decoder.decode(&bytes[..count], &mut words);
                assert_eq!((d.consumed, d.invalid_pairs), (count, 0));
                for &word in &words[..d.words] {
                    assert_eq!(word, received as u16 & 0x1ff);
                    received += 1;
                }
                c.commit_copy(c.consumed, count).unwrap();
            }
        }
        assert_eq!(received, sent / 2);
        assert_eq!(c.reloads, sent as u32 / DMA_RELOAD_COUNT);
        assert!(c.reloads >= 2);
    }

    #[test]
    fn reset_restarts_queue_position_and_partial_pair_together() {
        let mut c = Cursor::new();
        c.observe(DMA_RELOAD_COUNT - 300).unwrap();
        let mut decoder = NineBitPairDecoder::new();
        decoder.decode(&[0xaa], &mut []);
        c = Cursor::new();
        decoder.reset();
        c.observe(DMA_RELOAD_COUNT - 2).unwrap();
        let mut words = [0];
        assert_eq!(decoder.decode(&[1, 1], &mut words).words, 1);
        assert_eq!(words, [0x101]);
        assert_eq!(c.available(), Ok(2));
    }
}

#[derive(Clone, Copy, Default)]
pub struct QueueStats {
    pub rx_highwater: [u32; 2],
    pub rx_overwrites: [u32; 2],
    pub rx_reloads: [u32; 2],
    pub rx_max_gap_us: [u32; 2],
    pub tx_blocks: [u32; 2],
    pub tx_max_us: [u32; 2],
    pub uart_errors: u32,
    pub uart_error_bits: u32,
}
impl QueueStats {
    pub const fn new() -> Self {
        Self {
            rx_highwater: [0; 2],
            rx_overwrites: [0; 2],
            rx_reloads: [0; 2],
            rx_max_gap_us: [0; 2],
            tx_blocks: [0; 2],
            tx_max_us: [0; 2],
            uart_errors: 0,
            uart_error_bits: 0,
        }
    }
    pub fn encode(&self) -> heapless::Vec<u8, 68> {
        let mut bytes = heapless::Vec::new();
        bytes.extend_from_slice(&[1, 0, 0, 0]).unwrap();
        for value in [crate::uart_timing::ESP_DATA_BAUD_RATE, RX_RING_WORDS as u32]
            .into_iter()
            .chain(self.rx_highwater)
            .chain(self.rx_overwrites)
            .chain(self.rx_reloads)
            .chain(self.rx_max_gap_us)
            .chain(self.tx_blocks)
            .chain(self.tx_max_us)
            .chain([self.uart_errors, self.uart_error_bits])
        {
            bytes.extend_from_slice(&value.to_le_bytes()).unwrap();
        }
        bytes
    }
}
