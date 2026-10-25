use crate::host_uart::{HostRx, HostTx, HostUart};
use crate::{
    pio_uart::{AsicRx, AsicTx},
    runtime,
};
use bonanza_bridge_fw::queue::TRANSFER_BYTES;
use bonanza_bridge_fw::{
    transport::{Activity, Code, Fault, Phase, Source, RESET_TIMEOUT_MS, RX_QUIET_US, SERVICE_INTERVAL_MS},
    uart_codec::NineBitPairDecoder,
};
use embassy_futures::{
    select::{select3, Either3},
    yield_now,
};
use embassy_time::{with_timeout, Duration};

#[embassy_executor::task]
pub async fn transport_task(mut serial: HostUart, mut tx: AsicTx, mut rx: AsicRx) {
    let mut decoder = NineBitPairDecoder::new();
    tx.stop();
    serial.start();
    rx.start();
    tx.start();
    runtime::update(|s| s.phase = Phase::Running);
    loop {
        let token = if runtime::snapshot().phase == Phase::Running {
            // A single owner cancels BOTH forwarding futures before resetting.
            // This drops their local chunks and partial write positions. No
            // generation can race a UART write after its reset acknowledgment.
            let (serial_tx, serial_rx) = (&mut serial.tx, &mut serial.rx);
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

async fn reset_stream(serial: &mut HostUart, tx: &mut AsicTx, rx: &mut AsicRx, decoder: &mut NineBitPairDecoder) -> Result<(), Fault> {
    tx.stop();
    decoder.reset();
    rx.stop().await;
    serial.reset().await;

    // The ESP must stop/drain its data transmitter and hold ASIC reset before
    // sending RESET_STREAM. Drain every queued input byte and pending UART
    // error, then require a quiet interval longer than an entire UART frame.
    let mut discarded = [0u8; 64];
    loop {
        match with_timeout(Duration::from_micros(RX_QUIET_US), serial.rx.read(&mut discarded)).await {
            Err(_) => break,
            Ok(Ok(0)) => return Err(Fault::new(Source::Recovery, Code::Io)),
            Ok(Ok(count)) => runtime::update(|s| s.counters.reset_discarded_esp_bytes = s.counters.reset_discarded_esp_bytes.wrapping_add(count as u32)),
            Ok(Err(error)) => {
                runtime::update(|s| s.record(error));
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

#[cfg(feature = "bench-source")]
async fn esp_to_asic(serial: &mut HostRx, tx: &mut AsicTx, _decoder: &mut NineBitPairDecoder) -> Fault {
    // Bench-only full-rate return-path source. ASIC power must remain OFF.
    // One host byte starts 1 MiB of deterministic data through the actual PIO
    // transmitter, receiver, receive DMA, and host UART TX DMA.
    const SOURCE_WORDS: usize = 4096;
    let mut trigger = [0; TRANSFER_BYTES];
    loop {
        let count = match serial.read(&mut trigger).await {
            Ok(count) => count,
            Err(fault) => return fault,
        };
        runtime::update(|s| s.counters.esp_rx_bytes = s.counters.esp_rx_bytes.wrapping_add(count as u32));
        let fill = |words: &mut [u16], first: u32| {
            for (offset, word) in words.iter_mut().enumerate() {
                let index = first + offset as u32;
                let x = index.wrapping_mul(2654435761);
                *word = ((x ^ (x >> 16)) as u8 as u16) | (((index >> 8) as u16 & 1) << 8);
            }
        };
        let mut a = [0u16; SOURCE_WORDS];
        let mut b = [0u16; SOURCE_WORDS];
        let (mut sending, mut filling) = (&mut a, &mut b);
        fill(sending, 0);
        for first in (0u32..1024 * 1024).step_by(SOURCE_WORDS) {
            if let Err(fault) = serial.check_capture() {
                return fault;
            }
            runtime::service(Activity::Tx);
            // Generate the next block while DMA sends this one, so the fixture
            // exercises the wire limit instead of the CPU's generation rate.
            let (written, ()) = embassy_futures::join::join(with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), tx.write(sending)), async {
                fill(filling, first + SOURCE_WORDS as u32);
            })
            .await;
            match written {
                Err(_) => return Fault::new(Source::AsicTx, Code::Timeout),
                Ok(Err(fault)) => return fault,
                Ok(Ok(())) => {}
            }
            runtime::update(|s| s.counters.asic_tx_words = s.counters.asic_tx_words.wrapping_add(SOURCE_WORDS as u32));
            core::mem::swap(&mut sending, &mut filling);
            yield_now().await;
        }
    }
}

#[cfg(not(feature = "bench-source"))]
async fn esp_to_asic(serial: &mut HostRx, tx: &mut AsicTx, decoder: &mut NineBitPairDecoder) -> Fault {
    let mut input = [0u8; TRANSFER_BYTES];
    let mut words = [0u16; TRANSFER_BYTES / 2];
    loop {
        runtime::service(Activity::Tx);
        let count = match with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), serial.read(&mut input)).await {
            Err(_) => continue, // Idle is healthy; the partial pair is preserved.
            Ok(Ok(0)) => {
                yield_now().await;
                continue;
            }
            Ok(Ok(count)) => count,
            Ok(Err(error)) => return error,
        };
        runtime::update(|s| s.counters.esp_rx_bytes = s.counters.esp_rx_bytes.wrapping_add(count as u32));
        let decoded = decoder.decode(&input[..count], &mut words);
        assert_eq!(decoded.consumed, count);
        if decoded.invalid_pairs != 0 {
            runtime::update(|s| s.counters.invalid_pairs = s.counters.invalid_pairs.wrapping_add(decoded.invalid_pairs as u32));
            return Fault::new(Source::PairDecoder, Code::InvalidPair);
        }
        if decoded.words != 0 {
            match with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), tx.write(&words[..decoded.words])).await {
                Err(_) => return Fault::new(Source::AsicTx, Code::Timeout),
                Ok(Err(fault)) => return fault,
                Ok(Ok(())) => {}
            }
            runtime::update(|s| s.counters.asic_tx_words = s.counters.asic_tx_words.wrapping_add(decoded.words as u32));
        }
        // Ready UART/PIO futures under sustained traffic must still yield to
        // the reset request, the other direction, control, and the watchdog.
        yield_now().await;
    }
}

async fn asic_to_esp(serial: &mut HostTx, rx: &mut AsicRx) -> Fault {
    let mut bytes = [0u8; TRANSFER_BYTES];
    loop {
        let count = match rx.receive(&mut bytes).await {
            Ok(count) => count,
            Err(fault) => return fault,
        };
        runtime::service(Activity::Rx);
        match with_timeout(Duration::from_millis(SERVICE_INTERVAL_MS), serial.write(&bytes[..count])).await {
            Err(_) => return Fault::new(Source::EspUart, Code::Timeout),
            Ok(Err(error)) => return error,
            Ok(Ok(())) => runtime::update(|s| s.counters.esp_tx_bytes = s.counters.esp_tx_bytes.wrapping_add(count as u32)),
        }
        yield_now().await;
    }
}
