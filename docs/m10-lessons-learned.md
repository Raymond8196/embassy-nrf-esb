# M10 Lessons Learned: MPSL Timeslot + ESB + BLE

Created: 2026-05-20

This document summarizes the traps, false starts, debugging signals, and practical rules learned while bringing up M10. It is intentionally written as a learning/reference note rather than a clean implementation plan.

## Current Baseline

- Branch: `feat/mpsl-timeslot`.
- PRX + BLE example: `examples/mpsl_prx_ble.rs`.
- BLE-only diagnostic: `examples/mpsl_ble_connectable.rs`.
- PTX timeslot example: `examples/mpsl_ptx_in_slot.rs`.
- PRX timeslot example without BLE: `examples/mpsl_prx_in_slot.rs`.
- Latest verified status: `ESB M10` can connect and stay connected in nRF Connect, but ESB ACK coverage under an active BLE connection is currently poor.

## Big Picture Lessons

### Advertising Visibility Is Not Connection Stability

Seeing `ESB M10` in nRF Connect only proves the controller is advertising. It does not prove the BLE peripheral path is complete.

What happened:

- Advertising-only `mpsl_prx_ble` was visible.
- Full PRX timeslot + advertising remained visible.
- Changing to connectable advertising initially made the device either disappear or show up but fail to stay connected.
- The final working connectable path required both controller role support and minimal host-side ATT/L2CAP behavior.

Rule:

- Treat BLE validation as separate layers: visible advertisement, connectable advertisement, stable connection, ATT discovery, GATT service behavior, then ESB coexistence.

### BLE Controller Is Not a BLE Host

`nrf-sdc` is a controller-level integration. It can advertise and accept link-layer connections, but nRF Connect expects enough host behavior to complete service discovery.

What happened:

- `ADV_IND` plus `support_peripheral()` was not enough for the nRF Connect smoke test.
- Adding `peripheral_count(1)` got closer, but nRF Connect still did not stay connected cleanly.
- USB CDC logs showed repeated ATT `Read By Group Type` requests.
- After adding minimal ATT discovery responses for Generic Access and Device Name, `ESB CONN` connected and stayed connected.

Rule:

- For nRF Connect, provide at least a minimal ATT/L2CAP responder, even if the test only says "connect".
- A real product should use a BLE host stack instead of hand-written ATT once requirements grow beyond diagnostics.

### Separate BLE-Only From BLE+ESB

When BLE connection failed inside `mpsl_prx_ble`, it was unclear whether the failure was BLE configuration, host behavior, or ESB timeslot interference.

What worked:

- Add `mpsl_ble_connectable`, which runs MPSL + SDC connectable advertising without ESB timeslots.
- Bring up BLE connection there first.
- Merge only the proven minimal responder into `mpsl_prx_ble` afterward.

Rule:

- Do not debug BLE host behavior and ESB coexistence in the same step. Prove the BLE-only path first.

## MPSL Timeslot Lessons

### MPSL RADIO IRQ Routing Was Simpler Than Expected

Initial plan assumed a custom RADIO IRQ router might be needed. In practice, MPSL delivers RADIO work through `MPSL_TIMESLOT_SIGNAL_RADIO` during a timeslot.

Rule:

- Inside MPSL timeslots, route radio progress through the timeslot callback rather than trying to install a separate normal RADIO interrupt handler.

### TIMER0 Is Owned By MPSL During Timeslots

MPSL high-priority handling uses `RADIO`, `TIMER0`, and `RTC0` interrupt paths.

Pitfall:

- Shared state accessed from task context and the TIMER0 callback must be guarded in a way that is safe for the MPSL callback context.

What this branch uses:

- A `Timer0RawMutex` that masks/unmasks `TIMER0` while touching callback state.

Rule:

- Do not use abstractions that assume normal task context inside the high-priority timeslot callback.

### Timeslot Callbacks Must Return Quickly And Precisely

The MPSL callback return action controls whether the current slot ends, chains, or requests another slot.

Pitfall:

- Wrong return action or slow cleanup can cause invalid returns, slot loss, or overstay.

Rule:

- Keep callbacks deterministic.
- Set `callback_action` explicitly on every path.
- Clear timer/radio events before rearming.
- End or chain from the TIMER0 compare path, not from arbitrary task code.

### BLOCKED/CANCELLED Need Retry Policy

Timeslot requests can be blocked or cancelled by higher priority radio activity.

What worked:

- On `BLOCKED` / `CANCELLED`, retry with high priority and `MPSL_TIMESLOT_EARLIEST_TIMEOUT_MAX_US`.

Rule:

- Treat blocked/cancelled as expected scheduling feedback, not exceptional failure.

### OVERSTAYED Should Not Panic In Product Code

Current MPSL paths still panic on `OVERSTAYED`.

Pitfall:

- Panic is useful during bring-up, but a keyboard firmware must not crash because a radio slot overran.

Rule:

- Convert `OVERSTAYED` into counters plus safe termination before productizing.

## ESB-In-Timeslot Lessons

### RADIO Must Be Fully Reinitialized Per Slot

MPSL and SDC both use the radio. A timeslot user must not assume ESB register state survives.

What worked:

- At timeslot start: power-cycle RADIO, then rerun ESB register init.
- Restore protocol state such as PID and duplicate tracking after reinitialization.

Rule:

- Assume RADIO registers are not yours outside the current timeslot.

### PID Continuity Matters Across Slot Boundaries

ESB duplicate detection depends on PID/CRC continuity. Resetting PID or duplicate tracking at each slot creates false duplicates or missed retransmission behavior.

Pitfall found in review backlog:

- PRX TIMER0 handling creates a fresh `EsbRadio::new(pac::RADIO)` and then saves PID/CRC state from it. Because a new instance starts with zeroed state, this can clear cross-slot duplicate tracking.

Rule:

- Store PID/CRC tracking in the timeslot state itself or preserve it from the active radio state before losing it.
- Add valid bits for duplicate detection; `pid=0, crc=0` should not make the first packet look like a duplicate.

### Single Static DMA Buffers Are Fine For Smoke, Not API

The MPSL PTX/PRX paths use static 256-byte DMA buffers.

Why this was acceptable:

- It kept bring-up simple.
- Only one PTX or PRX session runs in the smoke examples.

Why it is not enough long-term:

- It is not re-entrant.
- It cannot support general queued traffic, multiple sessions, or a reusable public API.

Rule:

- Keep static buffers for diagnostic examples, but use owned buffers or packet pools for a real MPSL adapter.

### Free Functions Backed By Global State Are Not Re-entrant

`run_ptx_slots()` and `run_prx_slots()` are free functions backed by static state.

Pitfall:

- Concurrent calls would overwrite request, waker, config, counters, and buffers.

Rule:

- Add an owning handle or at least a busy guard before exposing this as a stable API.

## Multi-Pipe ACK Lessons

### Pipe 1 ACK Failure Was Not Caused By BLE

Initial Step 7 run showed pipe 0 ACKs and pipe 1 got `ack=0` while BLE advertising was active.

Follow-up:

- Running `mpsl_prx_in_slot` without BLE reproduced the same pipe 1 failure.

Conclusion:

- The root cause was in the MPSL PRX/PTX multi-pipe ACK path, not BLE coexistence.

Rule:

- Always reproduce a BLE coexistence failure without BLE before blaming the scheduler.

### Hardware Auto-ACK Is Too Fast For Software TXADDRESS Selection

In PRX mode, hardware `DISABLED -> TXEN` auto-ACK can start ACK TX before software reads `RXMATCH` and programs `TXADDRESS`.

Observed behavior:

- Hardware auto-ACK worked for pipe 0 but pipe 1 failed.
- ADDRESS-event preselect did not fix pipe 1.
- Manual ACK, where software reads `RXMATCH` before starting ACK TX, made pipe 1 ACKs appear.

Rule:

- For MPSL multi-pipe PRX ACK, prefer manual ACK unless there is a proven timing-safe way to set TXADDRESS before hardware starts ACK TX.

### Manual ACK Fixes Pipe Selection But Costs Slot Budget

Manual ACK added enough turnaround overhead to expose slot-boundary loss.

Observed runs:

- Manual ACK confirmed both pipes can ACK.
- Dense 10-packet bursts could lose a slot worth of packets.
- 12 ms slot with 11.5 ms match improved substantially.
- Current 14 ms examples can run both pipes but throughput varies.

Rule:

- Manual ACK is correctness-oriented.
- Slot length, packet density, and BLE coexistence need separate throughput tuning.

### ACK Counter Interpretation Can Be Misleading

In early pipe 0 runs, `ctr=1` with many ACK payloads looked suspicious.

Root issue:

- PRX ACK counter resets if the PRX session is restarted repeatedly.

Fix:

- Keep PRX in one long-lived `run_prx_slots(..., count=u32::MAX, ...)` session so per-pipe ACK counters stay monotonic.

Rule:

- If ACK payloads arrive but counters do not progress, check whether the PRX session is being restarted.

## BLE Connectable Lessons

### Required Pieces For The Diagnostic Connectable Path

Working BLE-only diagnostic uses:

- `support_adv()`.
- `support_peripheral()`.
- `peripheral_count(1)`.
- Larger SDC memory buffer than advertising-only path (`8192` bytes in examples).
- `SetEventMask` with LE meta and disconnection events enabled.
- `LeSetEventMask` with connection complete, enhanced connection complete, connection update complete, and remote connection parameter request enabled.
- `ADV_IND` advertising type.
- Minimal ATT/L2CAP/SMP handling.

Rule:

- Do not assume advertising-only SDC setup can become connectable by changing only `AdvKind`.

### nRF Connect Performs ATT Discovery Immediately

nRF Connect does not just connect and sit idle. It performs ATT discovery.

Observed via USB CDC logs:

- Repeated `ACL ATT len=7 opcode=16` meant repeated ATT `Read By Group Type Request`.
- The app appeared to connect but would not become stable until the responder returned valid discovery responses.

Minimum responses that worked:

- Exchange MTU response.
- Read By Group Type response for Generic Access primary service.
- Read By Type response for Device Name characteristic declaration.
- Read response for Device Name value.
- Find Information responses for known handles.
- Error responses for unsupported/unknown ranges.
- Pairing Failed for SMP Pairing Request.
- Basic L2CAP Connection Parameter Update response.

Rule:

- If nRF Connect loops or disconnects after link establishment, instrument ACL and ATT before changing radio scheduling.

### USB CDC Logging Is Useful But Can Hide Early Logs

The BLE diagnostic added USB CDC logging after it became clear defmt alone was inconvenient for interactive phone-side testing.

Pitfall:

- Logs emitted before the USB host opens CDC can be dropped if using a bounded channel.
- USB CDC has its own connection semantics; no output does not always mean firmware is dead.

Rule:

- Use USB CDC logs for interactive event traces, but verify basic USB enumeration separately.
- Keep log volume bounded; repetitive ATT loops can flood CDC output quickly.

## BLE + ESB Coexistence Lessons

### BLE Connection Stability Can Pass While ESB Throughput Fails

Latest combined run:

```text
pipe=0 tx=50 ack=0 ackpl=0 ctr=0 inv=0 blk=0 can=0
pipe=1 tx=67 ack=15 ackpl=15 ctr=29 inv=0 blk=0 can=0
DONE
```

Result:

- `ESB M10` connected and stayed connected.
- ESB PRX timeslot ACK coverage regressed severely.

Rule:

- Step 7 has two independent gates: BLE connection stability and ESB receive/ACK coverage under BLE connection.
- Passing the BLE gate does not imply the ESB gate is healthy.

### Active BLE Connection Takes Much More Radio Time Than Advertising

Advertising-only coexistence was good enough to get both pipes ACKing:

```text
pipe=0 tx=294 ack=271 ackpl=271 ctr=271 inv=0 blk=0 can=0
pipe=1 tx=359 ack=341 ackpl=341 ctr=342 inv=0 blk=0 can=0
DONE
```

Active connection coexistence was much worse.

Rule:

- Tune with an active connection, not only advertising.
- Connection interval, slave latency, MPSL priority, slot length, and packet density are all scheduling variables.

### The Next Tuning Target Is Scheduling, Not More BLE Host Code

Once `ESB M10` can stay connected and nRF Connect can discover the minimal Generic Access service, the immediate bottleneck moved to radio scheduling.

Likely tuning axes:

- BLE connection interval.
- BLE slave latency.
- ESB slot length.
- ESB in-slot TIMER0 match time.
- PTX packets per slot.
- MPSL timeslot priority and retry policy.
- Whether PRX should request shorter/frequent slots or longer/less frequent slots.

Rule:

- Do not keep adding ATT/GATT behavior until the radio duty-cycle problem is measured.

### Relaxed BLE Connection Parameters Restore ESB Budget

Requesting relaxed BLE connection parameters after connection complete materially improved ESB ACK coverage.

Measured run with requested CI=100 ms, latency=4, supervision timeout=6 s:

```text
pipe=0 tx=446 ack=444 ackpl=444 ctr=444 inv=0 blk=0 can=0
pipe=1 tx=258 ack=233 ackpl=233 ctr=234 inv=0 blk=0 can=0
DONE
```

Compared to the earlier active-connection run, pipe 0 recovered to near baseline and pipe 1 became useful again.

Rule:

- After BLE link establishment, request relaxed connection parameters for coexistence smoke tests before changing ESB radio logic.
- Treat connection interval and slave latency as first-class radio budget controls.
- Pipe 1 still needs tuning even after relaxed BLE parameters, so do not consider this final Step 7 throughput.

## USB / DFU / Hardware Workflow Lessons

### DFU Serial Names Change After Flashing

On macOS, the DFU bootloader and app CDC ports can have different names.

Observed examples:

- DFU ports: `/dev/tty.usbmodemC2A1EFA145C41`, `/dev/tty.usbmodemDC08665938A21`.
- App CDC port: `/dev/tty.usbmodem21301` or `/dev/tty.usbmodem21201`.

Rule:

- Always list `/dev/tty.*` and `/dev/cu.*` before and after flashing.
- Do not assume the same port remains after DFU.

### PRX May Not Expose USB CDC

`mpsl_prx_ble` does not expose USB CDC. It is validated through phone BLE visibility/connection and the PTX peer's CDC output.

Rule:

- If only one app CDC port appears after flashing both dongles, that is expected when PTX is the only USB CDC firmware.

### PTX CDC Output Requires The Firmware To Reach `wait_connection()`

No CDC output can mean several different things:

- PTX firmware is not running.
- It is still in DFU.
- USB CDC is enumerated but the class is waiting for connection/open state.
- The code progressed past logging before the host opened the port.
- The paired PRX is not running, so ACK counters show low/no progress but the final report still prints.

Rule:

- Check USB product string with system USB tools or `serial.tools.list_ports`.
- Open the app CDC port after flashing PTX and wait for the final `pipe=...` lines.

### Unsigned DFU Packages Are Fine Only For This Bootloader Setup

The generated DFU zip warnings about no signature key are expected for the current unsigned bootloader workflow.

Rule:

- Do not treat the warning as a failure for these dongles.
- Do not use unsigned app packages as production guidance.

## Documentation And Process Lessons

### Keep Verification Logs Separate From Plans

`docs/m10-plan.md` is the ideal roadmap. `docs/m10-verification.md` records what actually happened.

This split was useful because many measured results differed from the original estimates:

- ESB cycle time was closer to about 1 ms than the optimistic 500 us estimate.
- Pipe 1 ACK failure was not BLE-related.
- Connectable BLE required host behavior that was not obvious from the advertising example.
- Active BLE connection radio load was far worse than advertising-only load.

Rule:

- Keep planned criteria and measured results in separate sections/files.

### Commit Small, Hardware-Verified Milestones

Useful commits in this branch captured:

- Step 4 PTX PID persistence.
- Step 5 PRX-in-timeslot.
- Step 6 multi-pipe ACK payload.
- Manual ACK for MPSL PRX multi-pipe.
- Advertising coexistence re-run.
- Connectable BLE diagnostic and merged Step 7 connection first pass.

Rule:

- Commit buildable checkpoints after hardware observations, even when the result is partial or negative.

## Known Remaining Problems

### ESB Throughput Under Active BLE Connection

Current active BLE connection run has poor ACK coverage.

Next actions:

- Tune BLE connection interval and slave latency.
- Try longer/lower-density ESB slots while connected.
- Compare `mpsl_prx_ble` active connection run against advertising-only run with identical ESB slot parameters.
- Log MPSL counters (`blocked`, `cancelled`, `timer0`, `radio`) around the combined run.

### MPSL Stopgap Correctness

Review backlog still applies:

- Convert `OVERSTAYED` panic to safe counters/termination.
- Fix PRX PID/CRC state preservation.
- Add MPSL re-entry guard.
- Use `EsbHeader` helpers instead of manual header bit writes.

### Protocol/API Correctness

Review backlog still applies:

- Per-pipe ACK payload queue.
- Per-packet TX pipe metadata.
- Duplicate detection valid bits.
- Bound `EsbRadio::stop()` waits.
- Align fallback ACK buffer.

### BLE Host Layer

The minimal ATT/L2CAP responder is a diagnostic bridge, not a final BLE stack.

Next actions:

- Decide whether to integrate `trouble-host` or keep a minimal custom host for the narrow keyboard use case.
- Add real GATT echo/notify once radio scheduling is healthy.
- Avoid growing hand-written ATT beyond diagnostic needs unless product requirements justify it.

## Practical Debug Checklist

When a future M10 run fails, check in this order:

1. Is the right branch checked out?
2. Are both dongles in the expected mode: DFU or app?
3. Which serial ports exist before flashing?
4. Did both DFU commands report `Device programmed.`?
5. Which app ports exist after flashing?
6. Does the BLE advertiser appear?
7. If connectable, does nRF Connect stay connected before ESB starts?
8. Does the PTX app CDC enumerate with product `MPSL PTX in slot`?
9. Does PTX print final pipe stats?
10. If pipe 1 fails, reproduce without BLE before blaming coexistence.
11. If BLE connects but ESB throughput drops, tune scheduling before changing ATT/GATT.
12. If ATT loops, log ACL opcodes and add the missing discovery response.

## Useful Commands

Build/check examples:

```bash
cargo check --target thumbv7em-none-eabihf --example mpsl_ble_connectable --features nrf52840,defmt,mpsl
cargo check --target thumbv7em-none-eabihf --example mpsl_prx_ble --features nrf52840,defmt,mpsl
cargo check --target thumbv7em-none-eabihf --example mpsl_ptx_in_slot --features nrf52840,defmt,mpsl
```

Build/package examples:

```bash
cargo build --target thumbv7em-none-eabihf --release --example mpsl_prx_ble --features nrf52840,defmt,mpsl
rust-objcopy -O ihex target/thumbv7em-none-eabihf/release/examples/mpsl_prx_ble mpsl_prx_ble.hex
python3 -m nordicsemi pkg generate --application mpsl_prx_ble.hex --hw-version 52 --sd-req 0x00 --application-version 1 mpsl_prx_ble_dfu.zip
```

Flash examples:

```bash
python3 -m nordicsemi dfu serial --package mpsl_prx_ble_dfu.zip --port /dev/tty.usbmodemXXXX --baud-rate 115200 --flow-control 0
python3 -m nordicsemi dfu serial --package mpsl_ptx_in_slot_dfu.zip --port /dev/tty.usbmodemYYYY --baud-rate 115200 --flow-control 0
```

List ports:

```bash
ls /dev/tty.* /dev/cu.*
python3 -m serial.tools.list_ports -v
```

Read PTX CDC output:

```bash
python3 -c 'import serial, time; p="/dev/cu.usbmodemXXXX"; s=serial.Serial(p, 115200, timeout=0.5); s.dtr=True; s.rts=True; deadline=time.time()+30
while time.time()<deadline:
    chunk=s.read(256)
    if chunk: print(chunk.decode("utf-8", "replace"), end="")'
```
