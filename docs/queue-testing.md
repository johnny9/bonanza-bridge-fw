# 5 Mbaud queue regression

Normal images use 5 Mbaud on the ESP data UART and require an ESP image configured
for the same rate. The existing 2 Mbaud ESP image and original bridge are a
paired rollback; do not mix their baud rates. The control UART remains 115200.

Run the native tests and target lint described in the README. The queue tests use
production buffer sizes, validate ordered byte pairs across ring and transfer
counter wraps, reject overwritten copy windows, preserve UART error flags before
pair decoding, and reset partial pairs with the receive cursor. RX capture uses
hardware-chained DMA reloads, so a software task need not rearm a stopped stream.

For hardware regression, `bench-loopback` connects the actual TX GPIO8 to the
receive state machine internally. The entire path is ESP UART RX DMA, pair
decoder, ASIC TX DMA and PIO, ASIC RX PIO and DMA, then ESP UART TX DMA. No external
loopback wire is needed. `bench-source` uses the same loopback but generates one
MiB of deterministic 9-bit words after a host trigger, to exercise the return
path at the full ASIC wire rate. The source keeps both receive DMA counters
serviced. Both features automatically mark the image version as a bench image.

**Keep ASIC power off and reset asserted throughout these tests.** Restore a
normal bridge image before resuming mining or restarting the ESP. Bench images
are not miner firmware. They require the separate ESP queue-load fixture, whose
HTTP handler verifies paused mining and asserted ASIC reset. The fixture's
`/api/system/bridge-load` endpoint checks every received byte against the expected
sequence; it is deliberately absent from the normal ESP build.

Build and package each bench image separately, recording its identity before
programming. For example:

```sh
cargo build --release --locked --features bench-loopback
python3 scripts/package_firmware.py --out dist/loopback
python3 scripts/qualify_queues.py bonanza1004.local --mode loopback --runs 8 --out loopback.json

cargo build --release --locked --features bench-source
python3 scripts/package_firmware.py --out dist/source
python3 scripts/qualify_queues.py bonanza1004.local --mode source --runs 8 --out source.json
```

A normal bridge image can also run the direct regression for the original
receive overrun, still with ASIC power OFF:

```sh
python3 scripts/qualify_queues.py bonanza1004.local --mode flood --runs 8 --out flood.json
```

This sends 131,072 all-zero encoded bytes at 5 Mbaud. Both bytes of every pair
are valid even if a character is lost, so a UART overrun is reported directly
rather than being obscured by a later pair-decoder fault. The fixed bridge must
accept every input byte and forward every word without an error.

The script does not program images or control board power. Each loopback request
sends 2 MiB and expects 1 MiB back, crossing two host DMA reloads and one ASIC DMA
reload. Each source request expects 1 MiB back and crosses an ASIC DMA reload.
The script checks byte identity, transfer rate, both queue-overwrite counters,
UART and PIO errors, successful DMA reloads, byte counters, and bridge uptime.
Requests remain below five seconds: the temporary ESP HTTP fixture did not
reliably deliver responses for the longer eight-second probe, even though bridge
counters showed all bytes forwarded. Do not count a missing HTTP result as a
successful byte check.

After restoring normal firmware, test four-chip startup, clean jobs, sequence
reuse, pause/resume, and ESP-only restart. Mining is a separate test from raw
throughput: it supplies real ASIC responses but does not saturate both links.
These checks do not replace signal-integrity measurements or a long soak.
