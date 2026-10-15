//! Streaming ESP byte-pair decoder for 9-bit ASIC words.
#[derive(Default)]
pub struct NineBitPairDecoder {
    pending_low_byte: Option<u8>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Decoded {
    pub consumed: usize,
    pub words: usize,
    pub invalid_pairs: usize,
}

/// Explicit PUSH after nine right-shifted data bits leaves payload in 31:23.
pub const fn asic_rx_word_to_u16(word: u32) -> u16 {
    ((word >> 23) & 0x1ff) as u16
}
pub const fn asic_rx_word_to_u8(word: u32) -> u8 {
    asic_rx_word_to_u16(word) as u8
}

impl NineBitPairDecoder {
    pub const fn new() -> Self {
        Self { pending_low_byte: None }
    }
    pub fn reset(&mut self) {
        self.pending_low_byte = None;
    }
    pub const fn has_pending_byte(&self) -> bool {
        self.pending_low_byte.is_some()
    }

    /// Preserve split pairs. Report malformed bit-8 bytes, and never silently
    /// discard input when the output slice is too small. The firmware faults
    /// the stream on malformed pairs and requires a clean reset boundary.
    pub fn decode(&mut self, input: &[u8], output: &mut [u16]) -> Decoded {
        let mut result = Decoded { consumed: 0, words: 0, invalid_pairs: 0 };
        for &byte in input {
            if let Some(low) = self.pending_low_byte {
                if byte <= 1 {
                    if result.words == output.len() {
                        break;
                    }
                    output[result.words] = low as u16 | ((byte as u16) << 8);
                    result.words += 1;
                    self.pending_low_byte = None;
                } else {
                    result.invalid_pairs += 1;
                    self.pending_low_byte = Some(byte);
                }
            } else {
                self.pending_low_byte = Some(byte);
            }
            result.consumed += 1;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn words_preserve_all_nine_bits_and_raw_rx_drops_only_bit_eight() {
        let mut decoder = NineBitPairDecoder::new();
        let mut output = [0; 3];
        assert_eq!(decoder.decode(&[0x55, 1, 0xaa, 0, 0xff, 1], &mut output), Decoded { consumed: 6, words: 3, invalid_pairs: 0 });
        assert_eq!(output, [0x155, 0xaa, 0x1ff]);
        assert_eq!(asic_rx_word_to_u16(0x1a5 << 23), 0x1a5);
        assert_eq!(asic_rx_word_to_u8(0x1a5 << 23), 0xa5);
    }
    #[test]
    fn split_pairs_survive_idle_reads_and_arbitrary_chunk_boundaries() {
        let input = [0, 1, 1, 0, 0xff, 1];
        for split in 0..=input.len() {
            let mut decoder = NineBitPairDecoder::new();
            let mut output = [0; 3];
            let n = decoder.decode(&input[..split], &mut output).words;
            assert_eq!(decoder.decode(&[], &mut []).words, 0);
            assert_eq!(decoder.decode(&input[split..], &mut output[n..]).words + n, 3);
            assert_eq!(output, [0x100, 1, 0x1ff]);
            assert!(!decoder.has_pending_byte());
        }
    }
    #[test]
    fn reset_discards_partial_pairs_even_when_new_low_byte_is_zero_or_one() {
        for next in [0, 1, 0x80] {
            let mut decoder = NineBitPairDecoder::new();
            let mut output = [0; 1];
            decoder.decode(&[0xaa], &mut output);
            assert!(decoder.has_pending_byte());
            decoder.reset();
            assert_eq!(decoder.decode(&[next, 1], &mut output).words, 1);
            assert_eq!(output[0], 0x100 | next as u16);
            assert!(!decoder.has_pending_byte());
        }
    }
    #[test]
    fn invalid_bit_bytes_are_counted_instead_of_masked_into_valid_words() {
        let mut decoder = NineBitPairDecoder::new();
        let mut output = [0; 2];
        let result = decoder.decode(&[0x55, 0xaa, 0, 0xff, 1], &mut output);
        assert_eq!(result, Decoded { consumed: 5, words: 2, invalid_pairs: 1 });
        assert_eq!(output, [0xaa, 0x1ff]);
    }
    #[test]
    fn small_output_buffer_leaves_remaining_input_for_the_caller() {
        let mut decoder = NineBitPairDecoder::new();
        let input = [0xaa, 1, 0xbb, 0, 0xcc, 1];
        let mut output = [0; 1];
        let first = decoder.decode(&input, &mut output);
        assert_eq!(output[0], 0x1aa);
        assert_eq!(first.words, 1);
        let second = decoder.decode(&input[first.consumed..], &mut output);
        assert_eq!(output[0], 0xbb);
        let third = decoder.decode(&input[first.consumed + second.consumed..], &mut output);
        assert_eq!(output[0], 0x1cc);
        assert_eq!(first.consumed + second.consumed + third.consumed, input.len());
    }
}
