# Bonanza 1004 bridge firmware

RP2040 serial bridge for the Bonanza **1004x** hardware, reviewed at hardware
commit `a0c96ae0a20a75879d128308e84aea824c3d7da0` on the public `1104x` branch of
[bitaxeorg/bitaxeBonanza](https://github.com/bitaxeorg/bitaxeBonanza/tree/1104x).
The hardware is marked 1004x; the branch spelling is not the firmware board ID.

This branch is `feat/1004-uart`. The 1002 no-lease implementation is preserved
on `feat/1002-no-lease` at `6363665`. The earlier working source and original
binary-source references remain tagged `1002-lease-working-20261006` and
`1002-binary-source-20261006`.

The 1004 bridge starts forwarding at boot. It has no host lease, heartbeat,
GPIO/fan commands, trip input, or mining-session lifecycle. The ESP controls
ASIC power, reset, trip handling, and cooling. RP2040 GPIO10/11/18/20/21 are
unconnected on the reviewed 1004 design and are not configured by this firmware.

## Interfaces

| Interface | RP2040 TX / RX | ESP TX / RX | Format / rate |
|---|---|---|---|
| Control UART0 | GPIO0 / GPIO1 | GPIO43 / GPIO44 | 115200, 8N1 |
| Data UART1 | GPIO4 / GPIO5 | GPIO17 / GPIO18 | 5 Mbaud, 8N1 |
| ASIC PIO1 | GPIO8 / GPIO9 | — | 5 Mbaud, 9N1 |

ESP-to-ASIC data uses pairs `[low eight bits, bit 8]`; the second byte must be
exactly 0 or 1. ASIC-to-ESP data uses raw low-eight-bit bytes. A malformed pair
faults the stream instead of silently masking the invalid byte. A partial pair
survives idle periods and is cleared by `RESET_STREAM`.

ASIC RX starts without a power/reset gate. Its PIO program requires idle after
boot/reset and validates the stop bit before pushing a word. Low/unpowered
input does not produce synthetic zero bytes. Paced DMA drains RX into a
4096-word ring; the CPU validates each copied window against concurrent DMA
overwrite. UART RX also uses a 4096-word DMA ring, retaining each byte's UART
error bits. Each ring has a separate DMA reload channel so capture continues
across counter rollover without waiting for the executor. Both output paths
send blocks with DMA instead of awaiting and accounting for individual words.

At 5 Mbaud, the host ring holds 8.192 ms of full-rate input; the ASIC ring holds
9.011 ms at its 9N1 wire limit. These are bounded queues: overflow faults the
stream rather than continuing with a discontinuous byte sequence. UART flags
are checked before pair decoding. `GET_QUEUES` exposes queue high-water marks,
overflows, reloads, maximum polling gaps and block-transmit durations.

Both directions share one transport owner. A reset cancels both forwarding
futures, discards their local chunks and the partial pair, clears PIO/DMA state,
aborts pending DMA transfers, drains the ESP UART FIFO/shift register, drains
ESP input to a quiet boundary, then resumes forwarding before acknowledging.
The separate control UART remains available for identity, diagnostics, and
recovery. See the [protocol and recovery contract](docs/1004-protocol.md).

## Identity and compatibility

- Target board: **1004**.
- Protocol: **3.0**, info schema 1, transport-status schema 1, queue-status schema 1.
- Image manifest: existing 96-byte schema 1, board 1004, bridge kind 1,
  protocol 3.0, build-derived version, and manifest CRC-32.
- Firmware package: `0.1.0-1004.1`; builds append the Git revision and `.dirty`
  when applicable. `BONANZA_BRIDGE_FW_VERSION` remains available as an override.

Protocol 1/2 GPIO, fan, safety, arm, heartbeat, clear-fault, and safe-off
commands are rejected. The 1002 ESP driver and bridge updater do not support
this target. The ESP integration must recognize protocol 3/board 1004, use
direct board GPIOs, and follow the reset/reinitialization sequence in the
protocol document. A successful bridge reset does not establish ASIC or rail
readiness.

## Build and package

With Rust, the RP2040 target, and LLVM tools installed:

```sh
rustup target add thumbv6m-none-eabi
rustup component add rustfmt clippy llvm-tools
cargo fmt --all -- --check
cargo clippy --lib --target x86_64-unknown-linux-gnu --locked -- -D warnings
cargo test --lib --target x86_64-unknown-linux-gnu --locked
cargo clippy --bin bonanza-bridge-fw --target thumbv6m-none-eabi --locked -- -D warnings
cargo build --release --locked
python3 scripts/package_firmware.py
```

The packager produces `dist/bonanza-bridge-fw-1004.bin`, `.elf`, and `.json`.
It rejects an incorrect board/protocol, malformed version, duplicate manifest,
or bad manifest CRC before replacing the packaged binary. Use `--elf`, `--out`,
and `--objcopy` to select explicit paths. Packaging never flashes hardware.
CI builds the RP2040 image and validates its 1004 identity with the same script.

The ELF can be programmed through SWD with `probe-rs`. On 1004, ESP-side bridge
SWCLK/SWDIO moved to GPIO14/15. Existing 1002 updater pins must not be used.

## Validation boundary

Host tests execute the actual assembled RX PIO instructions against UART bit
streams, including all 512 words, back-to-back traffic, bad stops, low input,
and restart during a break. Other tests cover control framing, retired commands,
partial-pair recovery, DMA copy overruns, transport status, and watchdog health
without host traffic. The firmware also builds for `thumbv6m-none-eabi`.

These checks do not establish electrical operation on an assembled 1004 board.
Hardware qualification requires the paired 5 Mbaud ESP implementation, real UART
traffic and fresh ASIC responses, resets during traffic, and watchdog recovery.
The RP2040 watchdog cannot reset or power down the ASICs on this board.

Hardware queue regression instructions: [queue-testing.md](docs/queue-testing.md).
