//! The same assembled programs are used by the firmware and framing tests.
pub fn tx() -> pio::Program<32> {
    pio_proc::pio_asm!(
        ".side_set 1 opt"
        ".wrap_target"
        "pull side 1 [7]"
        "set x, 8 side 0 [7]"
        "bits:"
        "out pins, 1 [6]"
        "jmp x-- bits"
        ".wrap"
    )
    .program
}

pub fn rx() -> pio::Program<32> {
    pio_proc::pio_asm!(
        // Entry after boot/reset requires idle first. A low unpowered line
        // cannot become a start bit halfway through a break.
        "wait 1 pin 0"
        ".wrap_target"
        "start:"
        "wait 0 pin 0"
        "set x, 8 [10]"
        "bits:"
        "in pins, 1 [6]"
        "jmp x-- bits"
        // At the stop-bit center, 84 clocks after start detection.
        "jmp pin valid"
        "irq 4"
        "wait 1 pin 0"
        "jmp start"
        "valid:"
        "push block"
        ".wrap"
    )
    .program
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    // Execute the real RX bytecode, one PIO cycle at a time. Only the
    // instructions in this program are supported; an unexpected opcode fails.
    fn receive(levels: &[bool]) -> (Vec<u16>, usize) {
        let program = rx();
        let mut pc = 0usize;
        let mut delay = 0;
        let mut x = 0u32;
        let mut isr = 0u32;
        let mut words = Vec::new();
        let mut errors = 0;
        for &pin in levels {
            if delay != 0 {
                delay -= 1;
                continue;
            }
            let instruction = program.code[pc];
            let opcode = instruction >> 13;
            let argument = instruction & 0xff;
            let next = if pc == program.wrap.source as usize { program.wrap.target as usize } else { pc + 1 };
            let mut target = next;
            let mut stalled = false;
            match opcode {
                0 => {
                    let condition = (argument >> 5) & 7;
                    let jump = match condition {
                        0 => true,
                        2 => {
                            let nonzero = x != 0;
                            x = x.wrapping_sub(1);
                            nonzero
                        }
                        6 => pin,
                        _ => panic!("unsupported JMP condition"),
                    };
                    if jump {
                        target = (argument & 31) as usize;
                    }
                }
                1 => {
                    assert_eq!(argument & 0x7f, 0x20); // WAIT PIN 0
                    stalled = pin != (argument & 0x80 != 0);
                }
                2 => {
                    assert_eq!(argument, 1);
                    isr = (isr >> 1) | (u32::from(pin) << 31);
                }
                4 => {
                    assert_eq!(argument, 0x20);
                    words.push((isr >> 23) as u16);
                    isr = 0;
                }
                6 => {
                    assert_eq!(argument, 4);
                    errors += 1;
                }
                7 => {
                    assert_eq!(argument >> 5, 1);
                    x = (argument & 31) as u32;
                }
                _ => panic!("unsupported instruction"),
            }
            if !stalled {
                pc = target;
                delay = ((instruction >> 8) & 31) as usize;
            }
        }
        (words, errors)
    }

    fn frame(levels: &mut Vec<bool>, word: u16, stop: bool) {
        levels.extend([false; 8]);
        for bit in 0..9 {
            levels.extend([word & (1 << bit) != 0; 8]);
        }
        levels.extend([stop; 8]);
    }

    #[test]
    fn every_nine_bit_word_and_back_to_back_frames_keep_alignment() {
        for idle in 1..=8 {
            let mut levels = std::vec![true; idle];
            for word in 0..512 {
                frame(&mut levels, word, true);
            }
            levels.extend([true; 16]);
            let (words, errors) = receive(&levels);
            assert_eq!(words, (0..512).collect::<Vec<_>>());
            assert_eq!(errors, 0);
        }
    }

    #[test]
    fn idle_and_unpowered_low_lines_never_publish_words() {
        assert_eq!(receive(&[false; 10_000]), (Vec::new(), 0));
        assert_eq!(receive(&[true; 10_000]), (Vec::new(), 0));
        let mut levels = std::vec![true; 8];
        levels.extend([false; 10_000]);
        assert_eq!(receive(&levels), (Vec::new(), 1));
    }

    #[test]
    fn invalid_stop_is_discarded_and_recovery_waits_for_idle() {
        let mut levels = std::vec![true; 8];
        frame(&mut levels, 0x055, false);
        levels.extend([false; 200]);
        levels.extend([true; 8]);
        frame(&mut levels, 0x1a5, true);
        frame(&mut levels, 0x000, true);
        levels.extend([true; 8]);
        assert_eq!(receive(&levels), (std::vec![0x1a5, 0], 1));
    }

    #[test]
    fn restart_during_a_break_cannot_emit_a_partial_word() {
        let mut levels = std::vec![false; 30];
        levels.extend([true; 8]);
        frame(&mut levels, 0x100, true);
        levels.extend([true; 8]);
        assert_eq!(receive(&levels), (std::vec![0x100], 0));
    }
}
