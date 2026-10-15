use crate::runtime;
use bonanza_bridge_fw::{
    control_protocol::{self as protocol, Command, Error, Packet, MAX_REQUEST, PARTIAL_PACKET_TIMEOUT_MS},
    info, rx_stats,
    transport::{Activity, Code, Fault, Source, CONTROL_WRITE_TIMEOUT_MS, RESET_REPLY_TIMEOUT_MS},
};
use embassy_futures::yield_now;
use embassy_rp::{peripherals::UART0, uart::BufferedUart};
use embassy_time::{with_timeout, Duration, Timer};
use embedded_io_async::{Read, Write};

#[embassy_executor::task]
pub async fn uart_task(mut uart: BufferedUart<'static, UART0>) {
    let mut input = [0u8; MAX_REQUEST];
    let mut length = 0;
    let mut reset_token = 0u32;
    loop {
        runtime::service(Activity::Control);
        let packet = if let Some(packet) = protocol::extract(&mut input, &mut length) {
            Some(packet)
        } else {
            match with_timeout(Duration::from_millis(PARTIAL_PACKET_TIMEOUT_MS), uart.read(&mut input[length..])).await {
                Ok(Ok(count)) => {
                    length += count;
                    protocol::extract(&mut input, &mut length)
                }
                Ok(Err(error)) => {
                    record(runtime::uart_fault(Source::Control, error));
                    // Bytes preceding a UART error cannot prefix a new command.
                    length = 0;
                    None
                }
                Err(_) if length != 0 => {
                    let id = if length >= 3 { input[2] } else { 0xff };
                    length = 0;
                    Some(Packet { id, command: Err(Error::Timeout) })
                }
                Err(_) => None,
            }
        };
        if let Some(packet) = packet {
            let response = match packet.command {
                Ok(Command::GetInfo) => protocol::response(packet.id, &info::firmware_info().unwrap()),
                Ok(Command::GetRxStats) => {
                    let status = runtime::snapshot();
                    protocol::response(packet.id, &rx_stats::encode(status.counters.pio_stall_events, status.counters.ring_dropped_words))
                }
                Ok(Command::GetStatus) => protocol::response(packet.id, &runtime::snapshot().encode(runtime::now_ms())),
                Ok(Command::ResetStream) => {
                    reset_token = reset_token.wrapping_add(1);
                    match reset_stream(reset_token).await {
                        Ok(()) => protocol::response(packet.id, &runtime::snapshot().encode(runtime::now_ms())),
                        Err(error) => error_response(packet.id, error),
                    }
                }
                Err(error) => {
                    record(Fault::new(Source::Control, if error == Error::Timeout { Code::Timeout } else { Code::Invalid }));
                    error_response(packet.id, error)
                }
            };
            let result = with_timeout(Duration::from_millis(CONTROL_WRITE_TIMEOUT_MS), write_response(&mut uart, &response)).await;
            match result {
                Ok(Ok(())) => (),
                Ok(Err(fault)) => record(fault),
                Err(_) => record(Fault::new(Source::Control, Code::Timeout)),
            }
        }
        yield_now().await;
    }
}

fn record(fault: Fault) {
    runtime::update(|s| {
        s.counters.control_errors = s.counters.control_errors.wrapping_add(1);
        s.record(fault);
    });
}

fn error_response(id: u8, error: Error) -> heapless::Vec<u8, { protocol::MAX_RESPONSE }> {
    protocol::response(id, &[error as u8])
}

async fn reset_stream(token: u32) -> Result<(), Error> {
    runtime::RESET_REQUEST.try_send(token).map_err(|_| Error::Busy)?;
    let result = with_timeout(Duration::from_millis(RESET_REPLY_TIMEOUT_MS), async {
        loop {
            let (completed_token, result) = runtime::RESET_RESULT.wait().await;
            // Never turn a late completion of a timed-out request into the
            // acknowledgment for a different reset operation.
            if completed_token == token {
                return result;
            }
        }
    })
    .await
    .map_err(|_| {
        record(Fault::new(Source::Recovery, Code::Timeout));
        Error::Timeout
    })?;
    result.map_err(|fault| if fault.code == Code::Timeout { Error::Timeout } else { Error::Transport })
}

async fn write_response(uart: &mut BufferedUart<'static, UART0>, mut bytes: &[u8]) -> Result<(), Fault> {
    while !bytes.is_empty() {
        let count = uart.write(bytes).await.map_err(|e| runtime::uart_fault(Source::Control, e))?;
        if count == 0 {
            return Err(Fault::new(Source::Control, Code::ZeroWrite));
        }
        bytes = &bytes[count..];
    }
    uart.flush().await.map_err(|e| runtime::uart_fault(Source::Control, e))?;
    while uart.busy() {
        Timer::after_micros(20).await;
    }
    Ok(())
}
