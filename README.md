# bonanza-bridge-fw

bonanza-bridge-fw is RP2040 passthrough firmware for communicating with ASICs and board peripherals from a ESP32S3. The asic UART has been moved to PIO1 to support 9bit serial frames for the Intel BZM2 ASIC.

This firmware targets the [bitaxeBonanza-1002x](https://github.com/bitaxeorg/bitaxeBonanza/tree/1002x).

This branch implements protocol **2.0**, the no-lease firmware for **1002**.
The source baseline before this change is tagged `1002-lease-working-20261006`
(`dd480dd`). GPIO ownership remains on the RP2040. The existing protocol-1 ESP
firmware needs a matching update before it can use this build.

The local output interlock, host responsibilities, failure behavior, and
hardware limitations are documented in
[Bridge safety interlock](docs/safety-interlock.md).

## Developing

Install Rust:

```Shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

rustup target add thumbv6m-none-eabi

cargo install probe-rs-tools --locked
cargo install elf2uf2-rs --locked
cargo install cargo-binutils
```

Run the host-visible policy and protocol unit tests explicitly, because the repository's default Cargo target is the RP2040:

```Shell
cargo test --lib --target x86_64-unknown-linux-gnu
```

For SWD-based development and debugging:

```Shell
# Build the latest firmware:
cargo build --release

# Produce the raw image consumed by ESP-Miner's HTTP bridge updater:
arm-none-eabi-objcopy -O binary \
  target/thumbv6m-none-eabi/release/bonanza-bridge-fw \
  bonanza-bridge-fw.bin

# Build, program, and attach to the device with RTT for debugging:
cargo run --release

# Just flash the device, don't attach to RTT:
cargo flash --release --chip RP2040

# Erase all flash memory:
probe-rs erase --chip RP2040 --allow-erase-all
```

For UF2-based development:

```Shell
# Build the latest firmware:
cargo build --release

# Convert the ELF to an RP2040-compatible UF2 image:
elf2uf2-rs target/thumbv6m-none-eabi/release/bonanza-bridge-fw bonanza-bridge-fw.uf2

# Convert and deploy the UF2 image to an mounted RP2040:
elf2uf2-rs -d target/thumbv6m-none-eabi/release/bonanza-bridge-fw
```

## Running
The bonanza-bridge-fw firmware exposes two hardware UART interfaces to the ESP32S3 and one PIO-based 9-bit UART to the ASIC. USB serial is not used by this firmware.

### Serial Interfaces

| Interface | RP2040 Peripheral | RP2040 Pins | Format | Baudrate | Purpose |
|-----------|-------------------|-------------|--------|----------|---------|
| Control Serial | UART0 | TX: GPIO0, RX: GPIO1 | 8N1 | 115200 | Fan control and board control commands |
| Data Serial | UART1 | TX: GPIO4, RX: GPIO5 | 8N1 | 2000000 | ESP32S3-side BIRDS-compatible data stream |
| ASIC Serial | PIO1 | TX: GPIO8, RX: GPIO9 | 9N1 | 5000000 | BZM2 ASIC-side data stream |

### Data Serial
- Uses RP2040 UART1 on GPIO4/GPIO5.
- 8N1 on the ESP32S3 side and 9N1 on the BZM2 ASIC side.
- ESP-to-ASIC data is passed through as 9-bit words. ASIC-to-ESP data forwards
  the low eight response bits as raw bytes, matching the BIRDS receive path.
- Each direction runs in an independent async task. Paced DMA drains ASIC RX
  continuously into a naturally aligned 1024-word memory ring, so executor,
  interrupt, ESP command, or UART drain latency cannot stall the eight-entry
  PIO RX FIFO.
- The ASIC side remains fixed at 5000000 baud. The ESP link uses 2000000 baud,
  providing more than twelve times the measured raw receive payload budget
  while improving board-level signal margin.

**ESP32-to-ASIC 9-bit Data Encoding:**

Data sent from the ESP32S3 to the ASIC is encoded as pairs of bytes:
- **First byte**: Lower 8 bits of the 9-bit word (bits 0-7)
- **Second byte**: Bit 8 (only LSB is used, can be 0 or 1)

Examples:
- To send `0x155` (binary: `1_01010101`): Send bytes `[0x55, 0x01]`
- To send `0x0AA` (binary: `0_10101010`): Send bytes `[0xAA, 0x00]`
- Received ASIC data forwards only bits 0 through 7 as raw bytes. The ninth bit
  is still sampled so the receiver observes a complete 9N1 word, but it is not
  part of the BIRDS response parser contract.
- RX samples all nine bits at fixed eight-cycle intervals,
  with the first sample centered 1.5 bit periods after start detection. It
  requires the actual stop/idle level, and only then looks for the next start
  bit. Autopush occurs after all nine bits, and software then drops bit 8 before
  forwarding the byte to the ESP.

**Note:** The 9th bit can be used for addressing or protocol-specific purposes depending on your ASIC requirements.

### Control Serial
- Uses RP2040 UART0 on GPIO0/GPIO1.
- Format is 8N1.
- The firmware currently uses a fixed baudrate of 115200.

**Packet Format**

| 0      | 1      | 2  | 3   | 4    | 5   | 6... |
|--------|--------|----|-----|------|-----|------|
| LEN LO | LEN HI | ID | BUS | PAGE | CMD | DATA |

```
0. length low
1. length high
	- packet length is the number of bytes in the whole packet.
2. command id
	- Whatever byte you want. It will be returned in the response.
3. command bus
	- always 0x00
4. command page
	- System: 0x00
	- GPIO: 0x06
	- Fan: 0x09
5. command 
	- varies by command page. See below
6. data
	- data to write. variable length. See below
```

**Response Format**

Responses are also length-prefixed:

| 0      | 1      | 2  | 3... |
|--------|--------|----|------|
| LEN LO | LEN HI | ID | DATA |

- `ID` echoes the command ID from the request.
- `DATA` is the command response payload.
- Error responses use the same framing and return an error code in `DATA[0]`.

**Error Codes**

- `0x10`: timeout while receiving a packet
- `0x11`: invalid command or malformed packet
- `0x12`: safety policy denied the requested transition
- `0x13`: a safety fault is latched or the trip input is active

**Packet Timing**

- Control packets should be sent as a continuous byte stream.
- A partial control packet that stalls for more than a few milliseconds is treated as a timeout error.

**System**

Commands:

- get info: `0x01`
- get RX stats: `0x02`

The get-info response payload is:

| 0 | 1 | 2 | 3 | 4... |
|---|---|---|---|---|
| Schema version | Protocol major | Protocol minor | Version length | Version string |

- The current schema version is `1` and the control protocol version is `2.0`.
- The version string is printable ASCII and no more than 63 bytes.
- By default, builds report `<package-version>+g<short-git-sha>`, followed by `.dirty` when built from a modified checkout.
- Manufacturers can set `BONANZA_BRIDGE_FW_VERSION` at build time to use a release version instead.

### Image identity manifest

The RP2040 release binary embeds a fixed 96-byte BZM bridge image manifest in
flash. ESP-Miner scans and validates this manifest before it enters maintenance
or erases bridge flash. The manifest contains:

- the `BZM-BRIDGE-FW` magic and manifest schema;
- target board version `1002` and bridge firmware image kind;
- the bridge protocol major and minor version;
- the same build-derived firmware version returned by `GET_INFO`; and
- a CRC-32 covering the complete manifest.

ESP-Miner compares the manifest protocol and version with the running bridge
after programming and reset. The magic and CRC prevent accidental uploads of
unrelated or damaged RP2040 images; they are not a cryptographic signature
against a maliciously constructed image.

ASIC RX is drained continuously by a PIO-paced DMA channel into a 1024-word
address ring, then the normal buffered UART task forwards bounded raw-byte
chunks to the ESP. This is a correctness requirement at 5 Mbaud: the eight-word
PIO RX FIFO cannot hold a complete ten-word ASIC telemetry frame while an async
executor or interrupt-disabled critical section is busy. The ring covers more
than 100 complete TDM frames, reports exact overwrite loss, and its transfer
counter covers a 24-hour qualification run at the measured design rate. The RX
state machine and DMA channel are enabled only while 5 V is on and ASIC reset is
released; every safe transition synchronously aborts DMA, drains the FIFO, and
restores the program counter to its start-bit wait before the next enable.
There is no serial session or host timeout. Fan-only changes leave RX DMA and
its counters intact. This receive gate follows the bridge-owned 1002 GPIOs;
an always-active receiver for boards with ESP-owned GPIOs is a separate change.
This gate is not a full end-to-end flush of the UART buffers or TX pair decoder.

Example request with command ID `0x2a`:

`06 00 2a 00 00 01`

Example response for version `1.2.3`:

`0c 00 2a 01 02 00 05 31 2e 32 2e 33`

The get-RX-stats response is fixed-width and little-endian:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | RX stats schema, currently `1` |
| 1 | 4 | Cumulative PIO RX FIFO overflow count |
| 5 | 4 | Cumulative DMA software-ring overflow count |

Example request with command ID `0x2a`:

`06 00 2a 00 00 02`

Example zero-counter response:

`0c 00 2a 01 00 00 00 00 00 00 00 00`

### No-lease 1002 output policy

The bridge retains 5 V enable (GPIO18), ASIC reset (GPIO11), active-high trip
(GPIO10), fan PWM (GPIO20), and fan tach (GPIO21). GPIO and fan commands retain
their existing control-UART pages and payloads.

Before accepting commands, the bridge requests:

- 5 V disabled;
- ASIC reset asserted (`ASIC_RST/RST_N` low); and
- fan at 100 percent.

GPIO commands work directly without `ARM`, heartbeat, or a serial session.
Requested outputs persist when ESP control traffic stops. Status polling is
optional and never renews a timer. The active-high trip input still latches a
fault and restores the safe output request. `CLEAR_FAULT` requires trip low
and leaves the outputs safe; restarting then uses ordinary ordered GPIO writes.

The three-second RP2040 watchdog remains enabled. Only the control task feeds
it, after sampling trip and applying outputs. Control response writes remain
bounded to 100 ms; tach measurements service safety every 10 ms. A stalled
control task therefore causes firmware restart into safe boot initialization.
See [Bridge safety interlock](docs/safety-interlock.md) for host sequencing and
hardware limits.

Safety commands use system page `0x00` and have no request payload:

| Command | Value | Response |
|---|---:|---|
| Get safety status | `0x10` | 13-byte schema-2 status |
| Former arm lease | `0x11` | Invalid-command error `0x11`; opcode reserved |
| Former heartbeat | `0x12` | Invalid-command error `0x11`; opcode reserved |
| Clear safety fault | `0x13` | Requires trip low, restores safe outputs, returns status |
| Safe off | `0x14` | Restores safe outputs, preserves any latched fault, returns status |

`0x14` retains the safe-output action of the former disarm command. It does not
create or end a serial session. Successful `0x13` and `0x14` responses use the
same status schema as `0x10`.

Example get-status request with command ID `0x2a`:

`06 00 2a 00 00 10`

Status is little-endian for fields wider than one byte:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | Status schema `2` |
| 1 | 1 | Policy `3`: 1002 trip latch without lease |
| 2 | 1 | State: `0` safe-off, `1` controlled outputs, `2` fault-latched |
| 3 | 1 | Fault: `0` none, `2` ASIC-trip; old lease-expired value `1` reserved |
| 4 | 1 | Runtime: `0` good-safe-off, `1` good-controlled, `0x80` bad-fault, `0x82` bad-trip-input, `0x83` bad-unsafe-outputs |
| 5 | 1 | Production: `0` good, `0x81` capability-gap, `0x82` bad-runtime |
| 6 | 2 | Capability bits |
| 8 | 2 | Evidence bits |
| 10 | 1 | Output flags: bit 0 = 5 V enabled, bit 1 = reset asserted, bit 2 = fan at 100 percent |
| 11 | 1 | Commanded fan percentage |
| 12 | 1 | Sampled trip input, `0` or `1` |

There is no lease-remaining field. State describes the effective outputs and
fault, not host authorization. `CONTROLLED` means at least one output differs
from the safe request, including a reduced fan command while 5 V is off.

Example healthy safe-off response (16 bytes including framing):

`10 00 2a 02 03 00 00 00 81 8f 00 0d 00 06 64 00`

Capability bits retain their previous meanings:

| Bit | Capability | Current firmware |
|---:|---|---|
| 0 | 5 V control path | Set |
| 1 | ASIC reset control path | Set |
| 2 | Full-fan command path | Set |
| 3 | Trip input sampled by the control task | Set |
| 4 | Independent VCORE cutoff | Not set |
| 5 | Autonomous fan-tach interlock | Not set |
| 6 | Trip monitor independent of the command task | Not set |
| 7 | Controlled fan speed | Set |

Evidence bits 0, 2, and 3 mean outputs-safe, trip-clear, and fault-clear.
**Bit 1 (formerly lease-valid) is reserved and always clear.** Bits 4 through 6
mirror the independent hardware capabilities above. The current capability
value remains `0x008f`, so a healthy bridge reports production verdict `0x81`
(capability gap). This is separate from its runtime verdict.

Status reports effective commands, not physical rail measurements. The ESP
still owns VCORE, PGOOD, temperature, tachometer, and minimum-speed supervision.

### ESP compatibility

This firmware and its image manifest advertise protocol **2.0** and target
board **1002**. The manifest and get-info schemas remain `1`; the safety-status
schema changes to `2`.

The current ESP-Miner Bonanza code accepts major 1 and requires lease evidence.
It will reject this build. Its paired update must:

- recognize protocol 2 and decode the 13-byte status layout;
- remove arm, heartbeat, lease expiry, and lease-valid startup checks;
- retain trip, output, rail, temperature, and tach checks;
- retain GPIO/fan commands on the bridge for 1002;
- use safe-off during shutdown and establish safe outputs before a new startup;
- update bridge image validation to accept the intended protocol-2 image.

The existing protocol-1 decoder and updater must not simply bypass their
version checks. Protocol-1 and protocol-2 firmware have different control and
status contracts.

**GPIO**

Commands:

- 5v_en: 0x01
- asic_rst: 0x02
- asic_trip (read-only): 0x03

Data:

- [pin level] (omit for read operations)

Example:

- Set 5v_en High: `07 00 00 00 06 01 01`
- Get asic_rst: `06 00 00 00 06 02`
- Get asic_trip: `06 00 00 00 06 03`

The GPIO API exposes the electrical `asic_rst` pin level for compatibility.
Because this signal is `RST_N`, writing or reading `0` means reset is asserted.
Deasserting reset requires 5 V already enabled, and enabling 5 V requires reset
asserted and the fan command at 100 percent. No arm command is required.

**Fan**

Commands:

- set speed: 0x10
- get tachometer: 0x20

Data:

- [speed percentage 0-100] (for set speed command)

Example:

- Set fan speed to 50%:  `07 00 00 00 09 10 32`
- Set fan speed to 100%: `07 00 00 00 09 10 64`
- Read fan tach (RPM):   `06 00 00 00 09 20`

The fan must be at 100% before enabling 5 V. The host may apply a lower running
target and remains responsible for temperature, tachometer, and minimum-speed
supervision. Disabling 5 V, safe-off, clearing a fault, or an ASIC trip restores
100%. Host silence leaves the requested target unchanged.
