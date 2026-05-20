<!-- Licensed under the Apache-2.0 license -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# i2c userspace driver

Layered like `drivers/usart`, but adapted — patterns that solve a
usart-specific problem (e.g. parked/deferred reads) are intentionally **not**
carried over. i2c is strict request → response, run-to-completion.

**Supports:**
- **Master mode** — `embedded_hal::i2c::I2c::transaction()` (all operations;
  write to send MCTP packets). ✅ Complete.
- **Slave/target mode** — Interrupt-driven responder with event metadata for
  SPDM/MCTP dual-role operation. ✅ Slave RX (data reception) complete;
  ⚠️ Slave TX (ReadRequest responses) **required for SPDM responder** —
  see [SPDM responder implementation](#spdm-responder-implementation).

```
 consumer (any embedded-hal driver)
        │  embedded_hal::i2c::I2c            ← the only seam consumers see
        ▼
 client/        I2cClient<T: Transport> — ALL wire marshalling, host-buildable
        │  i2c_api::Transport  (bytes in → bytes out, one shot)
        ├── client-ipc/  IpcTransport  (production, cross-process, kernel)
        └── server/      LoopbackTransport (host tests + early boot)
                 │
                 ▼  i2c_server::dispatch — decodes, replays, scatters reads
            any embedded_hal::i2c::I2c
                 │
                 ▼
 target/ast10x0/backend/i2c/   ← platform-specific, lives under target/
        thin adapter over ast10x0_peripherals::i2c::Ast1060I2c
```

`api`/`client`/`server` are platform-agnostic and never name silicon. The
SoC-specific backend is the **only** crate that does, so it lives under
`target/<soc>/backend/`, mirroring `target/ast10x0/backend/usart`.

## Crates

| Crate | Bazel target | Host? | Role |
|-------|--------------|-------|------|
| `api` | `//drivers/i2c/api:i2c_api` | ✅ | Wire protocol + `embedded_hal::i2c::I2c` seam + the `Transport` seam. **Slave ops:** `ConfigureSlave`, `EnableSlave`, `DisableSlave`, `EnableSlaveNotification`, `SlaveReceive`, `SlaveSetResponse`. **Event kinds:** `DataReceived`, `ReadRequest`, `Stop` (for responder state machines). Host wire-codec tests. |
| `client` | `//drivers/i2c/client:i2c_client` | ✅ | `I2cClient<T: Transport>` implements `I2c` (master); also exposes slave methods (`configure_slave()`, `enable_slave()`, `slave_receive()`, `slave_receive_with_metadata()`, etc.). All marshalling, no kernel/IPC dep. |
| `client-ipc` | `//drivers/i2c/client-ipc:i2c_client_ipc` | ❌ embedded | `IpcTransport` (`channel_transact`). The one IPC-coupled client piece. |
| `server` | `//drivers/i2c/server:i2c_server` | ✅ | Pure `dispatch()` + `dispatch_slave()` + `LoopbackTransport`. Host dispatch + e2e tests (master + slave RX). |
| `server-runtime` | `//drivers/i2c/server-runtime:i2c_server_runtime` | ❌ embedded | The Pigweed WaitGroup wait/respond loop. One channel per bus. On slave-RX IRQ, latches buffer + metadata (event kind, source address) and raises `Signals::USER`. |
| `backend` (ast10x0) | `//target/ast10x0/backend/i2c:i2c_backend_ast10x0` (crate `i2c_backend`) | ❌ embedded | bus → reg-ptr map, `init_bus`, `open_bus`/`open_bus_dma`. Under `target/`. |

## Key invariants

- **Host-testable protocol (the structural-template property).** The client
  is generic over `i2c_api::Transport`; the *same* encoders/decoders run in
  production (`IpcTransport`) and in host tests (`LoopbackTransport` →
  `dispatch` → mock bus). Verified by `//drivers/i2c/tests:i2c_loopback_test`
  — consumer → client → loopback → dispatch → mock, **no kernel/QEMU**.
- **Atomicity preserved across the process boundary.** One client
  `I2c::transaction` ⇒ one `Transport::transact` ⇒ one server-side
  `I2c::transaction` ⇒ one response. Never fragmented per-op.
- **One IPC channel per bus.** Multi-bus lives entirely in the server:
  `i2c_server_runtime::run` takes `&[Bus { channel, driver }]`; adding a bus
  is one slice entry, no code change.
- **Server is backend-agnostic.** `dispatch`/`run` are generic over
  `embedded_hal::i2c::I2c`; never depend on the SoC backend. Errors map via
  the embedded-hal `ErrorKind` taxonomy.
- **Dual-role responder support.** Interrupt-driven slave RX with event
  metadata (kind + source address). Client waits on `Signals::USER`, fetches
  event via `slave_receive_with_metadata()`, stages response via
  `slave_set_response()`. Enables SPDM responders, register-echo patterns, and
  multi-master state tracking. See [Dual-role responder support](#dual-role-responder-support).

## Dual-role responder support

The template supports **SPDM requester and responder** dual-role operation via
the interrupt-driven slave API:

- **Master (requester):** `I2c::transaction()` for atomic request-response
  (write-read with repeated-START). Fully supported.

- **Slave (responder):** Interrupt-driven event notification + event metadata:
  - `enable_notification()` — arm IRQ; server raises `Signals::USER` on event
  - `slave_receive_with_metadata()` — non-blocking fetch after signal, returns
    `(event_kind, source_address, data_len)` plus scattered data into caller's
    buffer
  - `slave_set_response()` — pre-load TX buffer (one response at a time)

**Event metadata:**
- `event_kind` — `DataReceived`, `ReadRequest`, or `Stop` (for responder
  state machines)
- `source_address` — 7-bit I2C address of the requester (supports multi-master
  buses; may be `0xFF` if hardware doesn't track it)

**Responder pattern (SPDM, register-echo, etc.):**
```rust
client.configure_slave(0x50)?;
client.enable_slave()?;
client.enable_notification()?;

loop {
    // Wait for Signals::USER on channel
    object_wait_signal(&channel, USER_BIT)?;
    
    let event = client.slave_receive_with_metadata(&mut buf)?;
    // Use event.source_address for per-requester state
    // Use event.kind to distinguish read/write/stop
    
    // Stage response for master's next read
    client.slave_set_response(&buf[..event.data_len])?;
}
```

**Source address:** Captured during slave RX IRQ drain. Currently defaults to
`0xFF` (unavailable) until the hardware backend extracts it from registers.

## Wiring binaries

Client app (one bus): `I2cClient::new(IpcTransport::new(handle::I2C_n))` — then
use it purely as `embedded_hal::i2c::I2c`.

Server binary (no in-repo precedent yet — the runtime is a library, as with
`drivers/usart`):

1. once at boot: `ast10x0_board::Ast10x0Board::init()` — the **board crate**
   owns *all* I2C bring-up: subsystem (pin-mux / SCU clock+reset /
   `init_i2c_global`) **and** eager per-controller `init_bus` for every wired
   bus in its `&'static [I2cBusCfg]` descriptor (DMA buses included).
2. per owned bus, re-wrap the already-initialized controller (no re-init):
   `i2c_backend::open_bus(n, &cfg)` for BufferMode, or `open_bus_dma(n, &cfg,
   ram_nc_buf)` for DMA (buffer owned by the binary,
   `#[link_section=".ram_nc"]`). `&cfg` must be the **same** descriptor entry
   the board used.
3. build `&mut [i2c_server_runtime::Bus::new(handle::I2C_n, driver_n), …]`;
4. `i2c_server_runtime::run(handle::WG, handle::I2C_IRQ, signals::I2C, buses)`
   — the runtime also registers the i2c IRQ; on it, drains every
   notification-armed bus's slave RX into a per-bus latch and raises
   `Signals::USER` on that bus's channel.

The matching `system.json5` declares one `channel_handler` per bus, the i2c
interrupt object, and one `wait_group`. Deferred until a concrete board
target needs it — not fabricated speculatively.

## SPDM responder implementation

**Status:** ⚠️ **REQUIRED for ocp-emea demo — full SPDM responder, not a subset.**

The i2c driver supports SPDM requester/responder dual-role operation via
interrupt-driven event notification with metadata. A responder must:

```rust
client.configure_slave(RESPONDER_ADDR)?;
client.enable_slave()?;
client.enable_notification()?;

loop {
    // Wait for Signals::USER on the i2c channel
    object_wait(handle::I2C, Signals::USER, Instant::MAX)?;
    
    // Get event metadata: kind, requester address, data
    let event = client.slave_receive_with_metadata(&mut buf)?;
    
    match event.kind {
        SlaveEventKind::DataReceived => {
            // Master wrote to us; process SPDM request
            // Generate response in `response_buf`
            client.slave_set_response(&response_buf)?;
        }
        SlaveEventKind::ReadRequest => {
            // Master is about to read from us; we must have staged a response
            // via slave_set_response() in the prior cycle
        }
        SlaveEventKind::Stop => {
            // Transaction boundary; clear state if needed
        }
    }
}
```

**What's implemented:**
- ✅ Slave RX data reception
- ✅ Event metadata framework (fields exist but not populated)
- ✅ `slave_receive_with_metadata()` API (client-side)
- ✅ `slave_set_response()` API (wire protocol + server dispatch)
- ✅ Server-runtime interrupt drain + per-bus latch
- ✅ Wire protocol supports Stop events

**Critical gaps blocking full SPDM responder:**
- ❌ Event kind propagation — `rx_event_kind` hardcoded to `DataReceived`;
  responder cannot distinguish `Stop` event (transaction boundary)
- ❌ Source address extraction — `rx_source` hardcoded to `0xFF`; responder
  cannot identify which requester sent the message
- ❌ `poll_slave_event()` missing from HAL — backend must return event kind
  alongside rx length so server-runtime can store it

**Post-demo (not blocking):**
- ReadRequest event delivery — needed for responder state machines; baseline SPDM
  works via pre-staged TX buffer
- Hardware EVB testing (`--config=k_ast1060_evb`) — currently QEMU-only

Seam = `openprot_hal_blocking::i2c_hardware::slave` (`I2cSlaveCore`/`Buffer`),
reused. IRQ→USER notification path is QEMU-verified only (by decision); host
tests verify wire codec and server dispatch logic.

**Reference implementation:** See branch `openprot/ocp-emea-demo-stack-facade`
(commit `610d1f0`, "mctp: wire real I2C transport into server loop"). It shows
MCTP server wiring (WaitGroup multiplex, `get_pending_messages()`, `MctpI2cReceiver`).
**However:** that branch used aspeed-ddk as an external dependency, which has
architectural gaps (Stop/ReadRequest events dropped, no source address at driver
layer). The stack-facade MCTP layer works around this by extracting source address
from the MCTP-I2C payload byte[3], but that is not a substitute for proper driver
support. This repo owns the i2c peripheral code and must implement it correctly.

## Test matrix

| Test target | Tags | What it covers |
|-------------|------|----------------|
| `//drivers/i2c/api:i2c_api_test` | `host` | Wire-codec unit tests: `I2cRequestHeader`/`I2cResponseHeader` round-trips, `I2cOpDesc` encode/decode, error + opcode byte mapping stability, slave opcode round-trips (`ConfigureSlave`…`SlaveReceive`), `SlaveReceive` max-len in `op_count`, `NoData` status round-trip |
| `//drivers/i2c/server:i2c_server_test` | `host` | `dispatch()`: write+read round-trip through a mock bus, bus error → wire error-code mapping, short/malformed request rejected without panic. `dispatch_slave()`: configure/enable/disable apply to device, runtime-owned ops (`SlaveReceive`) and malformed requests rejected |
| `//drivers/i2c/tests:i2c_loopback_test` | `host` | End-to-end: consumer drives `I2cClient` purely through the `embedded_hal::i2c::I2c` seam; `LoopbackTransport` routes the **real** client encoders/decoders into `i2c_server::dispatch` onto an `EchoBus` mock. Verifies address, write payload, op ordering, read scatter, and slave-RX metadata — the exact marshalling path used in production |
| `//target/ast10x0/tests/peripherals/i2c/i2c_init:i2c` | `embedded` `qemu` | Full kernel/ARM system image build + boot: `Ast10x0Board::init()` brings up all wired I2C buses (incl. DMA); server-runtime loop starts; slave/notification IRQ path exercised |

**Critical gaps for SPDM (must fix before demo):**
- ⚠️ `ReadRequest` event delivery — backend's `handle_slave_interrupt()` must
  return `SlaveEvent::ReadRequest` and wake the client; currently only data
  events wake the client
- ⚠️ `Stop` event delivery — similarly, `SlaveEvent::Stop` must be returned and
  propagated to client
- ⚠️ Source address extraction — currently defaults to `0xFF`; backend must
  read requester address from hardware registers

**Not yet covered (lower priority):**
- Multi-message queue and per-address state tracking (responder handles one event
  at a time; multi-requester support TBD)
- Blocking semantics with timeouts (responder uses interrupt notification; no
  polling fallback yet)
- Hardware EVB (`--config=k_ast1060_evb`) — I2C is QEMU-verified only by current
  decision

## Status

Host tests (`bazel test`, no kernel/QEMU): `//drivers/i2c/api:i2c_api_test`
(wire codec incl. slave ops), `//drivers/i2c/server:i2c_server_test`
(`dispatch` + `dispatch_slave` + error map),
`//drivers/i2c/tests:i2c_loopback_test` (end-to-end client↔server marshalling).
Full kernel/ARM stack incl. the slave/notification path and the `i2c_init:i2c`
system image build under `--config=virt_ast10x0`; the `ast1060_pac`
instance-name assumption is **confirmed** (crates compiled clean).

## Design analyses

Compare openprot-i2c against reference implementations (Zephyr, DDK):

- **[ANALYSIS-external-behavior-arity.md](ANALYSIS-external-behavior-arity.md)** — Master/slave mode API comparison between openprot-i2c and Zephyr i2c_target. Identifies architectural differences (sync vs. async, IPC vs. kernel, whole-buffer vs. per-byte).

- **[ANALYSIS-register-access-patterns.md](ANALYSIS-register-access-patterns.md)** — Register access discipline: Confined-unsafe MMIO Façade (openprot-i2c) vs. distributed unsafe (DDK) vs. implicit (Zephyr). Also covers caller-controlled yield patterns.

- **[ANALYSIS-register-misuse.md](ANALYSIS-register-misuse.md)** — Three classes of register bugs fixed in the template vs. DDK: silicon errata (master/slave coexistence), field encoding (multi-byte TX truncation), atomicity (RX buffer race window).
