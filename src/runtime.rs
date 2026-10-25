use bonanza_bridge_fw::transport::{Activity, Code, Fault, Source, Status};
use core::cell::RefCell;
use embassy_sync::{
    blocking_mutex::{raw::CriticalSectionRawMutex, Mutex},
    channel::Channel,
    signal::Signal,
};
use embassy_time::{Instant, Timer};

static STATUS: Mutex<CriticalSectionRawMutex, RefCell<Status>> = Mutex::new(RefCell::new(Status::new()));
pub static RESET_REQUEST: Channel<CriticalSectionRawMutex, u32, 1> = Channel::new();
pub static RESET_RESULT: Signal<CriticalSectionRawMutex, (u32, Result<(), Fault>)> = Signal::new();

pub fn now_ms() -> u64 {
    Instant::now().as_millis()
}
pub fn update(f: impl FnOnce(&mut Status)) {
    STATUS.lock(|s| f(&mut s.borrow_mut()));
}
pub fn snapshot() -> Status {
    STATUS.lock(|s| *s.borrow())
}
pub fn service(activity: Activity) {
    update(|s| s.service(activity, now_ms()));
}

pub fn uart_fault(source: Source, error: embassy_rp::uart::Error) -> Fault {
    use embassy_rp::uart::Error;
    Fault::new(
        source,
        match error {
            Error::Framing => Code::Framing,
            Error::Parity => Code::Parity,
            Error::Overrun => Code::Overrun,
            Error::Break => Code::Break,
            _ => Code::Io,
        },
    )
}

#[embassy_executor::task]
pub async fn watchdog_task(mut watchdog: embassy_rp::watchdog::Watchdog) {
    loop {
        if snapshot().watchdog_healthy(now_ms()) {
            watchdog.feed();
        }
        Timer::after_millis(100).await;
    }
}

static QUEUES: Mutex<CriticalSectionRawMutex, RefCell<bonanza_bridge_fw::queue::QueueStats>> = Mutex::new(RefCell::new(bonanza_bridge_fw::queue::QueueStats::new()));
pub fn queues(f: impl FnOnce(&mut bonanza_bridge_fw::queue::QueueStats)) {
    QUEUES.lock(|s| f(&mut s.borrow_mut()));
}
pub fn queue_snapshot() -> bonanza_bridge_fw::queue::QueueStats {
    QUEUES.lock(|s| *s.borrow())
}
