# Bridge output interlock: no-lease 1002

## Scope

This firmware keeps the 1002 board-control GPIOs on the RP2040 and removes the
host output lease. The tagged source baseline is
`1002-lease-working-20261006` at `dd480dd0e6de306047aa3efb32ed159e889e4093`.

There is no arm command, heartbeat, session token, or host-traffic deadline.
Outputs persist until an explicit command, an ASIC trip, or bridge restart.
Stopping ESP traffic alone does not shut down the board. The RP2040 watchdog
monitors progress of its own control task, independently of ESP traffic.

The safe output request is 5 V disabled, ASIC reset asserted (`RST_N` low), and
fan at 100 percent. The bridge applies it at boot, on a sampled active-high
trip, on safe-off, on disabling 5 V, and after clearing a fault. Power/reset
ordering checks and the trip latch are retained.

These are effective RP2040 commands, not measurements of 5 V, VCORE, PGOOD,
temperature, or fan speed. The bridge has no independent VCORE cutoff or fan
stall interlock. Those responsibilities remain with the ESP and board hardware.

## Ownership

| Function | RP2040 pin |
|---|---:|
| 5 V enable | GPIO18 |
| ASIC reset, active low | GPIO11 |
| ASIC trip, active high | GPIO10 |
| Fan PWM, active low | GPIO20 |
| Fan tach input | GPIO21 |
| Control UART TX / RX, 115200 8N1 | GPIO0 / GPIO1 |
| ESP data UART TX / RX, 2 Mbaud 8N1 | GPIO4 / GPIO5 |
| ASIC PIO TX / RX, 5 Mbaud 9N1 | GPIO8 / GPIO9 |

`src/safety.rs` owns the pure output policy, trip latch, command decoding, and
status encoding. `src/control/mod.rs` samples trip before output commands,
applies the policy, and feeds the watchdog. GPIO and fan handlers keep their
existing pages and payloads. `src/main.rs` retains safe boot levels and pins.

For this 1002 step, ASIC RX remains enabled only with 5 V on and reset released.
The gate is independent of host traffic. Fan-only changes do not restart RX
DMA or reset receive counters. A power/reset gate transition still aborts DMA
and drains the RX FIFO. It does not flush every UART buffer or the TX pair
decoder; a separate `RESET_STREAM` operation is not implemented here.

Moving GPIOs to the ESP and making RX independent of those GPIOs are deferred
to the later 1004 implementation.

## Output states and transitions

| Reported state | Meaning |
|---|---|
| `SAFE_OFF` | No fault and all effective outputs match the safe request |
| `CONTROLLED` | No fault and at least one effective output differs from safe |
| `FAULT_LATCHED` | A trip fault forces safe outputs until explicitly cleared |

State describes outputs; it is not authorization or an acquired session.
Ordinary valid GPIO/fan commands are accepted immediately after boot.

The following commands remain available even with a fault latched: disable
5 V, assert reset, command full fan, and safe-off. They do not clear the fault.
`CLEAR_FAULT` requires the sampled trip input low and restores safe outputs;
there is no automatic restart when trip goes low.

Ordering is enforced without a lease:

1. Keep fan at 100 percent and reset asserted when enabling 5 V.
2. Enable 5 V before releasing reset.
3. Reduced fan commands require no latched fault. The ESP must provide its own
   temperature, tachometer, and minimum-speed supervision.

Rejected requests leave the output request unchanged. Disabling 5 V also
asserts reset and restores full fan. A reduced fan request while 5 V is off
reports `CONTROLLED` and must be restored to full before enabling 5 V.

## Host sequence and protocol

Use a matching protocol-2 ESP build. The current protocol-1 ESP implementation
requires a lease and rejects this firmware, including its image manifest.

1. Query `GET_INFO` and require supported protocol 2 and info schema 1.
2. Read schema-2 safety status. Establish safe outputs with `SAFE_OFF` before a
   fresh startup, including after an ESP-only restart while the bridge is live.
3. Resolve any trip fault; clear it only once trip is low. Confirm safe outputs
   and no fault before starting.
4. Enable 5 V with fan full and reset asserted.
5. Perform the ESP-owned rail and other startup checks, then release reset.
6. Apply the supervised running fan target. Poll status as desired; polling
   neither grants permission nor keeps a lease alive.
7. On shutdown, use `SAFE_OFF` and verify safe outputs. A retained fault still
   reports `FAULT_LATCHED`.

System page `0x00` commands are `GET_SAFETY_STATUS` (`0x10`), `CLEAR_FAULT`
(`0x13`), and `SAFE_OFF` (`0x14`). The former arm/heartbeat opcodes `0x11` and
`0x12` are reserved and return invalid-command error `0x11`.

The status is 13 bytes: schema 2, policy 3 (1002 trip latch without lease),
state, fault, runtime verdict, production verdict, two-byte capabilities,
two-byte evidence, output flags, fan percent, and sampled trip. It has no
lease counter. Former lease-valid evidence bit 1 stays clear. The complete
wire layouts and examples are in the [README](../README.md#no-lease-1002-output-policy).

Capabilities remain `0x008f`: 5 V, reset, full-fan control, trip sampling, and
controlled fan speed. Independent VCORE cutoff, autonomous fan-tach interlock,
and independent trip monitoring remain unclaimed. Consequently a healthy
bridge still reports production verdict `0x81` (capability gap), separately
from runtime health.

## Timing and failure behavior

| Mechanism | Bound |
|---|---:|
| RP2040 hardware watchdog | 3000 ms |
| Control UART response write | 100 ms |
| Safety service during a 500 ms tach measurement | Every 10 ms |
| Partial control packet wait | Approximately 4 ms |

Only the control task feeds the watchdog, after sampling trip and applying
outputs. Trip remains sampled by that task, not an independent hardware
monitor.

| Condition | Bridge response |
|---|---|
| ESP traffic stops | Keep requested outputs and service trip/watchdog normally |
| ASIC trip asserts | Latch ASIC-trip and request safe outputs |
| Invalid power/reset/fan sequence | Reject without applying the transition |
| Partial control packet stalls | Discard it and return timeout; outputs persist |
| Response cannot complete | Stop the write after 100 ms and continue servicing safety |
| Control task stalls | Watchdog resets firmware into safe boot initialization |

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --lib --target x86_64-unknown-linux-gnu --locked -- -D warnings
cargo test --lib --target x86_64-unknown-linux-gnu --locked
cargo build --release --locked
```

Host tests cover direct startup without arm, repeated service without host
commands, output ordering, trip latching and explicit recovery, safe-off,
status wire vectors, rejected legacy commands, preserved 1002 image identity,
and RX continuity across fan changes. The RP2040 build checks hardware wiring
against the pure policy APIs. Board testing with the paired ESP update is
still required to validate real GPIO, trip, watchdog, and serial behavior.
