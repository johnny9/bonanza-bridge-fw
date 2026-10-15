# Bonanza 1004 bridge protocol 3.0

## Ownership and normal operation

Only the UART bridge runs on the RP2040. ASIC reset, trip, switched 5 V, fan
PWM/tach, and the rest of board supervision belong to the ESP. No bridge command
claims a physical board-output change. There is no host lease, heartbeat,
session identifier, or normal start/stop operation. Both data directions start
at boot and remain active during idle periods and absent status polls.

The separate control UART uses 115200 baud/8N1. Data UART uses 2 Mbaud/8N1;
ASIC serial uses 5 Mbaud/9N1. ESP-to-ASIC encoding is `[low byte, bit 8]`;
ASIC-to-ESP bytes contain only bits 0..7. The wire formats are unchanged.

## Control framing

```text
Request:  length:u16LE | id:u8 | bus:u8 | page:u8 | command:u8
Response: length:u16LE | id:u8 | payload
```

Length includes the entire packet. Bus and page must both be zero. Commands
have no payload; current valid requests are exactly six bytes. One transaction
may be outstanding. The parser buffers at most 64 bytes, rejects invalid
lengths/bus/page/opcode/trailing data, preserves coalesced packets, and drops
partial state after a UART error or a four-millisecond inter-read timeout.
The framing does not contain a CRC. Match response IDs and validate the payload
schema and exact length; discard failed transactions rather than reuse status.

| Opcode | Command | Success payload |
|---:|---|---|
| `0x01` | `GET_INFO` | Info schema 1, protocol major 3, minor 0, version length, ASCII version |
| `0x02` | `GET_RX_STATS` | Schema 1 + u32 PIO stall observations + u32 DMA overwritten words |
| `0x03` | `GET_STATUS` | 84-byte transport status below |
| `0x04` | `RESET_STREAM` | Transport status after a completed reset |

All protocol-1/2 board-control commands are unsupported, including system
`0x10..0x14`, GPIO page `0x06`, and fan page `0x09`. Their response is error
`0x11`. Old opcodes are not successful no-ops.

Errors are single-byte payloads, using the normal response header:

| Value | Meaning |
|---:|---|
| `0x10` | Partial packet, reset, or completion timeout |
| `0x11` | Unsupported command or malformed packet |
| `0x12` | Reset request queue is busy |
| `0x13` | Transport operation failed |

Example `GET_STATUS` request with ID 42: `06 00 2a 00 00 03`.
A successful response starts `57 00 2a` (87 total bytes), then the status.
An unsupported command returns `04 00 2a 11`.

## Transport status

All multi-byte fields are little-endian. Counters and generation are u32 and
wrap modulo 2^32. They survive stream reset and clear on bridge reboot. PIO
flags count observations, not exact lost bytes. UART byte/word counters describe
software acceptance/queueing, not proof that an ASIC processed a command.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | Status schema 1 |
| 1 | 1 | Phase: 0 starting, 1 running, 2 resetting, 3 faulted |
| 2 | 1 | RP2040 watchdog reason register: bit 0 timer, bit 1 forced; zero means neither watchdog reason |
| 3 | 1 | Last error source |
| 4 | 1 | Last error code |
| 5 | 1 | Watchdog feed eligibility at this snapshot (0/1) |
| 6 | 2 | Board version 1004 |
| 8 | 4 | Transport capabilities `0x1f` |
| 12 | 8 | Bridge uptime in milliseconds |
| 20 | 4 | Stream generation, incremented only after a successful reset |
| 24 | 4 | Successful stream resets |
| 28 | 4 | Historical error-source bitmap: bit N corresponds to source N |
| 32 | 4 | ESP input bytes accepted by the normal forwarding task |
| 36 | 4 | 9-bit words queued to ASIC TX PIO |
| 40 | 4 | ASIC bytes copied from a valid DMA window |
| 44 | 4 | Bytes queued to ESP data UART TX |
| 48 | 4 | Invalid bit-8 bytes in ESP data pairs |
| 52 | 4 | ESP data UART read/write errors returned by the driver |
| 56 | 4 | Control parse/UART errors and reset completion-wait timeouts |
| 60 | 4 | PIO invalid-stop/break flag observations |
| 64 | 4 | PIO RX FIFO stall flag observations |
| 68 | 4 | DMA ring overwritten words |
| 72 | 4 | Automatic rearms after the finite DMA count ends |
| 76 | 4 | Failed reset operations |
| 80 | 4 | ESP input bytes discarded by reset cleanup |

Capability bits: 0 pair-encoded TX, 1 raw RX, 2 transport diagnostics,
3 coordinated stream reset, 4 local progress watchdog. These bits belong to
protocol 3 and must not be decoded as the old GPIO capability mask.

Error sources: 0 none, 1 control, 2 ESP data UART, 3 pair decoder, 4 ASIC RX,
5 ASIC TX, 6 RX DMA, 7 recovery. Error codes: 0 none, 1 I/O, 2 timeout,
3 invalid command, 4 framing, 5 parity, 6 overrun, 7 break, 8 invalid pair,
9 PIO FIFO stall, 10 DMA ring overwrite, 11 DMA bus/stopped-channel error,
12 zero-length UART write.

Historical errors remain visible after successful recovery. Use phase and
counter deltas to distinguish current operation from past errors. `RUNNING`
means the bridge forwarding tasks are active; it does not mean ASICs are
powered, enumerated, hashing, or safe.

## Stream reset and ESP integration

The two UARTs have independent ordering. Required ESP sequence:

1. Stop mining writes and hold the ASICs in reset using the ESP GPIO.
2. Drain the ESP data UART transmitter completely, including its hardware FIFO
   and shift register. Keep it silent until reset completion.
3. Send `RESET_STREAM` on the control UART with a fresh request ID.
4. Require the matching ID and valid status: schema 1, board 1004, phase running,
   and a successful generation change relative to the prior status when known.
5. While ASIC reset is still held, drain ESP data RX and clear the ASIC parser,
   pending results, and job associations. Old bytes may have arrived during
   the bridge's output drain, but none from before reset can follow its ACK.
6. Release reset, observe ASIC startup timing, enumerate, and resume mining.

Use this sequence on initial bring-up, ESP-only restart, ASIC reinitialization,
bridge watchdog reset, or a reported data-path fault. The ESP remains responsible
for physical power sequencing, trips, cooling, and shutting down on failures.

The bridge implements a reset as one operation owned by the transport task:

1. Cancel both forwarding futures, dropping local chunks/partial write state.
2. Clear the pair decoder and ASIC TX FIFO/shift state, holding TX idle high.
3. Disable ASIC RX, complete the DMA-abort handshake, and clear RX FIFO/state.
4. Flush the ESP-bound software queue **and wait for UART hardware TX idle**.
5. Drain buffered ESP input and pending UART errors until a 1 ms quiet interval.
6. Restart clean RX DMA/PIO and TX automatically, increment generation, and
   return status. Read-only commands never request these steps.

This clears all bridge-owned stream state. Its clean-boundary guarantee depends
on the ESP holding reset and draining/pausing its transmitter. The bridge cannot
verify those board conditions. A reset is repeatable; a lost response does not
mean it did not run. Keep ASIC reset asserted after a timeout, resolve/retry the
operation with a fresh ID, and reinitialize before resuming data.

Cleanup has a 100 ms deadline; the control task waits at most 250 ms for a
matching internal completion, and response writes have a 100 ms deadline.
Use a host control timeout of at least 500 ms for reset. A failure leaves the
ASIC forwarding paths stopped and reports failure, without advancing generation.
Late completions cannot acknowledge a different request. A stopped transport
can be retried through the control UART; otherwise the watchdog restarts it.

## Watchdog and data faults

The hardware watchdog has a 3 s timeout. A separate task feeds it only while
control, ESP-to-ASIC, and ASIC-to-ESP tasks have each serviced their work within
500 ms and phase is running/resetting. Idle reads service progress too; no
traffic or status-poll minimum exists. Data operations use 50 ms waits so a
stalled operation cannot indefinitely masquerade as healthy task progress.

Malformed ESP byte pairs, UART errors, blocked data output, and DMA bus/stopped
channel faults stop forwarding and report phase faulted. PIO invalid stops are
discarded; FIFO stall and ring overwrite observations remain diagnostic counters
while forwarding continues. An ASIC response parser must still detect and
recover from dropped/corrupt responses.

Watchdog reset affects the RP2040 only. It does not assert ASIC reset, cut power,
or force fan speed on 1004. The ESP must detect bridge failure/restart and run
the board recovery sequence above.

## Qualification still required on hardware

- Match info/status/image board and protocol before programming or power-up.
- Exercise UART with no polls, frequent polls, and sustained traffic both ways.
- Test ASIC power-off, reset, low RX, valid replies, and ring overrun reporting.
- Reset with a split byte pair, pending PIO TX, RX DMA data, and queued ESP output.
- Verify no stale output arrives after an acknowledged reset; test lost ACK,
  active ESP traffic during reset, reset timeout, and retry.
- Stall each bridge task, observe watchdog recovery, and reinitialize via ESP.
- Confirm unchanged control/data pins and actual 5 Mbaud timing on the board.

The bridge image does not include the ESP-Miner port. That port also needs the
1004 GPIO backend, protocol-3 decoder/commands, board-specific updater identity,
and new SWD pin mapping; accepting major 3 alone is insufficient.
