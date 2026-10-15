//! Transport state only. No GPIO ownership, host lease, or mining session.
use heapless::Vec;

pub const WATCHDOG_TIMEOUT_MS: u64 = 3_000;
pub const TASK_STALE_MS: u64 = 500;
pub const SERVICE_INTERVAL_MS: u64 = 50;
pub const CONTROL_WRITE_TIMEOUT_MS: u64 = 100;
pub const RESET_TIMEOUT_MS: u64 = 100;
pub const RESET_REPLY_TIMEOUT_MS: u64 = 250;
pub const RX_QUIET_US: u64 = 1_000;
pub const STATUS_SCHEMA: u8 = 1;
pub const STATUS_LENGTH: usize = 84;
// These are protocol-3 transport capabilities, not protocol-1/2 GPIO bits.
pub const CAPABILITIES: u32 = 0x1f; // pair TX, raw RX, diagnostics, reset, watchdog

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting = 0,
    Running = 1,
    Resetting = 2,
    Faulted = 3,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    None = 0,
    Control = 1,
    EspUart = 2,
    PairDecoder = 3,
    AsicRx = 4,
    AsicTx = 5,
    Dma = 6,
    Recovery = 7,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    None = 0,
    Io = 1,
    Timeout = 2,
    Invalid = 3,
    Framing = 4,
    Parity = 5,
    Overrun = 6,
    Break = 7,
    InvalidPair = 8,
    FifoStall = 9,
    RingOverwrite = 10,
    DmaBus = 11,
    ZeroWrite = 12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub source: Source,
    pub code: Code,
}
impl Fault {
    pub const fn new(source: Source, code: Code) -> Self {
        Self { source, code }
    }
}

#[derive(Clone, Copy)]
pub enum Activity {
    Control = 0,
    Tx = 1,
    Rx = 2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub esp_rx_bytes: u32,
    pub asic_tx_words: u32,
    pub asic_rx_bytes: u32,
    pub esp_tx_bytes: u32,
    pub invalid_pairs: u32,
    pub esp_uart_errors: u32,
    pub control_errors: u32,
    pub pio_framing_events: u32,
    pub pio_stall_events: u32,
    pub ring_dropped_words: u32,
    pub dma_rearms: u32,
    pub reset_failures: u32,
    pub reset_discarded_esp_bytes: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub phase: Phase,
    pub watchdog_reason: u8,
    pub last_error: Fault,
    pub error_sources: u32,
    pub generation: u32,
    pub resets: u32,
    pub counters: Counters,
    serviced: [Option<u64>; 3],
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

impl Status {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Starting,
            watchdog_reason: 0,
            last_error: Fault::new(Source::None, Code::None),
            error_sources: 0,
            generation: 0,
            resets: 0,
            counters: Counters {
                esp_rx_bytes: 0,
                asic_tx_words: 0,
                asic_rx_bytes: 0,
                esp_tx_bytes: 0,
                invalid_pairs: 0,
                esp_uart_errors: 0,
                control_errors: 0,
                pio_framing_events: 0,
                pio_stall_events: 0,
                ring_dropped_words: 0,
                dma_rearms: 0,
                reset_failures: 0,
                reset_discarded_esp_bytes: 0,
            },
            serviced: [None; 3],
        }
    }

    pub fn service(&mut self, activity: Activity, now_ms: u64) {
        self.serviced[activity as usize] = Some(now_ms);
    }

    /// Task progress, including idle polling, is required. Received bytes and
    /// host status/heartbeat messages are deliberately not part of this test.
    pub fn watchdog_healthy(&self, now_ms: u64) -> bool {
        matches!(self.phase, Phase::Running | Phase::Resetting) && self.serviced.iter().all(|last| last.is_some_and(|last| now_ms.saturating_sub(last) < TASK_STALE_MS))
    }

    pub fn record(&mut self, fault: Fault) {
        self.last_error = fault;
        self.error_sources |= 1 << fault.source as u8;
    }

    pub fn begin_reset(&mut self) {
        self.phase = Phase::Resetting;
    }

    pub fn finish_reset(&mut self, result: Result<(), Fault>) {
        match result {
            Ok(()) => {
                self.generation = self.generation.wrapping_add(1);
                self.resets = self.resets.wrapping_add(1);
                self.phase = Phase::Running;
            }
            Err(fault) => {
                self.counters.reset_failures = self.counters.reset_failures.wrapping_add(1);
                self.record(fault);
                self.phase = Phase::Faulted;
            }
        }
    }

    pub fn encode(&self, now_ms: u64) -> Vec<u8, STATUS_LENGTH> {
        let mut bytes = Vec::new();
        bytes
            .extend_from_slice(&[STATUS_SCHEMA, self.phase as u8, self.watchdog_reason, self.last_error.source as u8, self.last_error.code as u8, u8::from(self.watchdog_healthy(now_ms))])
            .unwrap();
        bytes.extend_from_slice(&crate::image_manifest::TARGET_BOARD_VERSION.to_le_bytes()).unwrap();
        bytes.extend_from_slice(&CAPABILITIES.to_le_bytes()).unwrap();
        bytes.extend_from_slice(&now_ms.to_le_bytes()).unwrap();
        let c = self.counters;
        for value in [
            self.generation,
            self.resets,
            self.error_sources,
            c.esp_rx_bytes,
            c.asic_tx_words,
            c.asic_rx_bytes,
            c.esp_tx_bytes,
            c.invalid_pairs,
            c.esp_uart_errors,
            c.control_errors,
            c.pio_framing_events,
            c.pio_stall_events,
            c.ring_dropped_words,
            c.dma_rearms,
            c.reset_failures,
            c.reset_discarded_esp_bytes,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes()).unwrap();
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_tasks_keep_watchdog_healthy_without_host_messages() {
        let mut status = Status::new();
        assert!(!status.watchdog_healthy(0));
        status.phase = Phase::Running;
        for now in (0..100_000).step_by(50) {
            for task in [Activity::Control, Activity::Tx, Activity::Rx] {
                status.service(task, now);
            }
            assert!(status.watchdog_healthy(now));
        }
        assert_eq!(status.counters, Counters::default());
        assert_eq!(status.generation, 0);
    }

    #[test]
    fn any_task_stall_or_transport_fault_stops_watchdog_feeds() {
        for stalled in 0..3 {
            let mut status = Status::new();
            status.phase = Phase::Running;
            for (i, task) in [Activity::Control, Activity::Tx, Activity::Rx].into_iter().enumerate() {
                status.service(task, if i == stalled { 0 } else { TASK_STALE_MS });
            }
            assert!(status.watchdog_healthy(TASK_STALE_MS - 1));
            assert!(!status.watchdog_healthy(TASK_STALE_MS));
            status.phase = Phase::Faulted;
            assert!(!status.watchdog_healthy(0));
        }
    }

    #[test]
    fn reset_is_one_operation_and_preserves_error_history() {
        let mut status = Status::new();
        status.phase = Phase::Running;
        status.counters.esp_rx_bytes = 300;
        let fault = Fault::new(Source::Recovery, Code::Timeout);
        status.begin_reset();
        assert_eq!(status.phase, Phase::Resetting);
        status.finish_reset(Err(fault));
        assert_eq!(status.phase, Phase::Faulted);
        assert_eq!(status.generation, 0);
        status.begin_reset();
        status.finish_reset(Ok(()));
        assert_eq!(status.phase, Phase::Running);
        assert_eq!(status.generation, 1);
        assert_eq!(status.resets, 1);
        assert_eq!(status.last_error, fault);
        assert_eq!(status.counters.esp_rx_bytes, 300);
        assert_eq!(status.counters.reset_failures, 1);
        for _ in 0..10 {
            let _ = status.encode(1234);
        }
        assert_eq!(status.generation, 1);
    }

    #[test]
    fn status_wire_offsets_identify_transport_and_error_source() {
        let mut status = Status::new();
        status.watchdog_reason = 1;
        status.record(Fault::new(Source::PairDecoder, Code::InvalidPair));
        status.counters.invalid_pairs = 0x12345678;
        let bytes = status.encode(0x0102030405060708);
        assert_eq!(bytes.len(), 84);
        assert_eq!(&bytes[..12], &[1, 0, 1, 3, 8, 0, 0xec, 3, 0x1f, 0, 0, 0]);
        assert_eq!(&bytes[12..20], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&bytes[28..32], &[8, 0, 0, 0]);
        assert_eq!(&bytes[48..52], &[0x78, 0x56, 0x34, 0x12]);
    }
}
