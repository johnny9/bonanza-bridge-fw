use crate::{
    pio_uart::{AsicRx, AsicTx},
    runtime,
};
use bonanza_bridge_fw::{
    transport::{Activity, Code, Fault, Phase, Source, RESET_TIMEOUT_MS, RX_QUIET_US, SERVICE_INTERVAL_MS},
    uart_codec::NineBitPairDecoder,
};
use embassy_futures::{
    select::{select3, Either3},
    yield_now,
};
use embassy_rp::{
    peripherals::UART1,
    uart::{BufferedUart, BufferedUartRx, BufferedUartTx},
};
use embassy_time::{with_timeout, Duration, Timer};
use embedded_io_async::{Read, Write};

#[embassy_executor::task]
pub async fn transport_task(mut serial: BufferedUart<'static, UART1>, mut tx: AsicTx, mut rx: AsicRx) {
    let mut decoder = NineBitPairDecoder::new();
    tx.stop();
    rx.start();
    tx.start();
    runtime::update(|s| s.phase = Phase::Running);
    loop {
        let token = if runtime::snapshot().phase == Phase::Running {
            // A single owner cancels BOTH forwarding futures before resetting.
            // This drops their local chunks and partial write positions. No
            // generation can race a UART write after its reset acknowledgment.
            let (serial_tx, serial_rx) = serial.split_ref();
            match select3(runtime::RESET_REQUEST.receive(), esp_to_asic(serial_rx, &mut tx, &mut decoder), asic_to_esp(serial_tx, &mut rx)).await {
                Either3::First(token) => Some(token),
                Either3::Second(fault) | Either3::Third(fault) => {
                    runtime::update(|s| {
                        s.record(fault);
                        s.phase = Phase::Faulted;
                    });
                    None
                }
            }
        } else {
            Some(runtime::RESET_REQUEST.receive().await)
        };

        if let Some(token) = token {
            runtime::update(|s| s.begin_reset());
            runtime::service(Activity::Tx);
            runtime::service(Activity::Rx);
            let result = with_timeout(Duration::from_millis(RESET_TIMEOUT_MS), reset_stream(&mut serial, &mut tx, &mut rx, &mut decoder))
                .await
                .unwrap_or(Err(Fault::new(Source::Recovery, Code::Timeout)));
            runtime::update(|s| s.finish_reset(result));
            runtime::RESET_RESULT.signal((token, result));
        } else {
            // A broken data path is visible through GET_STATUS. It can be
            // recovered explicitly; otherwise lack of transport progress lets
            // the hardware watchdog restart the bridge. Control stays alive.
            tx.stop();
            if with_timeout(Duration::from_millis(RESET_TIMEOUT_MS), rx.stop()).await.is_err() {
                runtime::update(|s| s.record(Fault::new(Source::Recovery, Code::Timeout)));
            }
            decoder.reset();
        }
        yield_now().await;
    }
}

async fn reset_stream(serial: &mut BufferedUart<'static, UART1>, tx: &mut AsicTx, rx: &mut AsicRx, decoder: &mut NineBitPairDecoder) -> Result<(), Fault> {
    tx.stop();
    decoder.reset();
    rx.stop().await;
    // Embassy flush drains the software buffer only. BUSY also covers the
    // hardware FIFO/shift register: old bytes must finish before the ACK.
    serial.flush().await.map_err(|e| runtime::uart_fault(Source::Recovery, e))?;
    while serial.busy() {
        Timer::after_micros(10).await;
    }

    // The ESP must stop/drain its data transmitter and hold ASIC reset before
    // sending RESET_STREAM. Drain every queued input byte and pending UART
    // error, then require a quiet interval longer than an entire UART frame.
    let mut discarded = [0u8; 64];
    loop {
        match with_timeout(Duration::from_micros(RX_QUIET_US), serial.read(&mut discarded)).await {
            Err(_) => break,
            Ok(Ok(0)) => return Err(Fault::new(Source::Recovery, Code::Io)),
            Ok(Ok(count)) => runtime::update(|s| s.counters.reset_discarded_esp_bytes = s.counters.reset_discarded_esp_bytes.wrapping_add(count as u32)),
            Ok(Err(error)) => {
                record_esp_error(error);
            }
        }
        yield_now().await;
    }
    // The sole owner restarts clean PIO/DMA state only after UART cleanup.
    // A timeout above leaves both ASIC directions stopped and reports failure.
    rx.start();
    tx.start();
    Ok(())
}

fn record_esp_error(error: embassy_rp::uart::Error) -> Fault {
    let fault = runtime::uart_fault(Source::EspUart, error);
    runtime::update(|s| {
        s.counters.esp_uart_errors = s.counters.esp_uart_errors.wrapping_add(1);
        s.record(fault);
    });
    fault
}

async fn esp_to_asic(serial: &mut BufferedUartRx<'static, UART1>, tx: &mut AsicTx, decoder: &mut NineBitPairDecoder) -> Fault {
    let mut input = [0u8; 64];
    let mut words = [0u16; 32];
    loop {
        runtime::service(Activity::Tx);
        let count = match with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), serial.read(&mut input)).await {
            Err(_) => continue, // Idle is healthy; the partial pair is preserved.
            Ok(Ok(0)) => {
                yield_now().await;
                continue;
            }
            Ok(Ok(count)) => count,
            Ok(Err(error)) => return record_esp_error(error),
        };
        runtime::update(|s| s.counters.esp_rx_bytes = s.counters.esp_rx_bytes.wrapping_add(count as u32));
        let decoded = decoder.decode(&input[..count], &mut words);
        assert_eq!(decoded.consumed, count);
        if decoded.invalid_pairs != 0 {
            runtime::update(|s| s.counters.invalid_pairs = s.counters.invalid_pairs.wrapping_add(decoded.invalid_pairs as u32));
            return Fault::new(Source::PairDecoder, Code::InvalidPair);
        }
        for word in &words[..decoded.words] {
            if with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), tx.write(*word)).await.is_err() {
                return Fault::new(Source::AsicTx, Code::Timeout);
            }
            runtime::update(|s| s.counters.asic_tx_words = s.counters.asic_tx_words.wrapping_add(1));
        }
        // Ready UART/PIO futures under sustained traffic must still yield to
        // the reset request, the other direction, control, and the watchdog.
        yield_now().await;
    }
}

async fn asic_to_esp(serial: &mut BufferedUartTx<'static, UART1>, rx: &mut AsicRx) -> Fault {
    let mut bytes = [0u8; 64];
    loop {
        let count = match rx.receive(&mut bytes).await {
            Ok(count) => count,
            Err(fault) => return fault,
        };
        let mut offset = 0;
        while offset < count {
            runtime::service(Activity::Rx);
            match with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), serial.write(&bytes[offset..count])).await {
                Err(_) => return Fault::new(Source::EspUart, Code::Timeout),
                Ok(Err(error)) => return record_esp_error(error),
                Ok(Ok(0)) => return Fault::new(Source::EspUart, Code::ZeroWrite),
                Ok(Ok(written)) => {
                    offset += written;
                    runtime::update(|s| s.counters.esp_tx_bytes = s.counters.esp_tx_bytes.wrapping_add(written as u32));
                }
            }
        }
        yield_now().await;
    }
}
