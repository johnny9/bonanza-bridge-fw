# 2026-10-25 board-1004 validation

The original installed bridge, `0.1.0-1004.1+g521b71a.dirty`, was preserved by
reading all 2 MiB of flash twice. Its image SHA256 is
`d6070c0aad7e96bccc3c72ed92854d1bc3665bfcbdc3eff0bc6a7555ef1166e0`.
The experiment changed its UART divider in RAM to exactly 5 Mbaud at 125 MHz.

With ASIC power off, sending 131,072 all-zero encoded bytes in 262,487 us caused
the original bridge to report UART overrun (source 2, code 6), with zero invalid
pairs. It accepted only 105,202 bytes and forwarded 52,601 words. The UART RSR
also reported overrun. This establishes receive/forwarding throughput loss at
5 Mbaud, independent of malformed input pairs. It does not isolate ISR latency
from software-ring backpressure within that original buffered-UART path.

The fixed implementation captures both receive streams continuously with DMA,
retains UART error bits until validation, and transmits blocks with DMA. Rings
hold 4,096 entries each. Hardware reload channels keep capture continuous across
transfer-count rollover. Overwrites and FIFO exhaustion fault the stream rather
than silently dropping bytes. No ESP TX-buffer increase is used.

On bonanza1004:

- Eight loopback runs sent 16 MiB at approximately 500 kB/s and verified all
  8 MiB of simultaneous return data. Both DMA reload boundaries were crossed
  in every run; no queue, UART, or PIO errors occurred.
- Eight sustained return-stream runs verified 8 MiB at a median 451,404 bytes/s,
  99.3% of the 5 Mbaud/9N1 wire limit. The ASIC receive queue peaked at 534 of
  4,096 entries, with no errors. These are separate maximum-rate and simultaneous
  loopback tests, not a claim of both independent directions saturated at once.
- The controlled mining run passed all six enabled cases, 120 seconds of clean
  job exclusion, another 120 seconds of renewal, and 70 rapid clean refills that
  forced sequence reuse. No retired shares were submitted after the barriers.
  Independent hashing verified 4,632 captured shares without duplicates.
- Mining receive high-water marks were 258 host entries and 10 ASIC entries.
  The observed result rate was 979 GH/s versus 1,007 GH/s expected at 800 MHz.
- All 34 bridge native tests and host/target lint passed. The paired ESP changes
  passed 10 BZM native tests and 21 public-header compilation checks.

Temporary fixtures exposed two test limitations: eight-second HTTP requests
sometimes lost their response despite complete bridge byte counts, so qualified
byte checks use requests shorter than five seconds; the initial synthetic source
omitted host DMA progress sampling and tripped the guard, which was corrected in
the fixture. Neither event is counted as a passing probe.

The qualification uses local HTTP/console evidence and deterministic byte checks.
It does not include oscilloscope signal-integrity measurements, a power-cycle by
physically removing power, or a long-duration soak. The ESP load/flash fixture
is isolated from the normal ESP firmware and is not required for mining. See
[queue-testing.md](queue-testing.md) for the repeatable regression procedure.
