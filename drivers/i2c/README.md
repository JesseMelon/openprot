<!-- Licensed under the Apache-2.0 license -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# i2c userspace driver

Layered like `drivers/usart`, but adapted — patterns that solve a
usart-specific problem (e.g. parked/deferred reads) are intentionally **not**
carried over. i2c is strict request → response, run-to-completion.

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
| `api` | `//drivers/i2c/api:i2c_api` | ✅ | Wire protocol + `embedded_hal::i2c::I2c` seam + the `Transport` seam. Host wire-codec tests. |
| `client` | `//drivers/i2c/client:i2c_client` | ✅ | `I2cClient<T: Transport>` implements `I2c`; **all** marshalling. No kernel/IPC dep. |
| `client-ipc` | `//drivers/i2c/client-ipc:i2c_client_ipc` | ❌ embedded | `IpcTransport` (`channel_transact`). The one IPC-coupled client piece. |
| `server` | `//drivers/i2c/server:i2c_server` | ✅ | Pure `dispatch()` + `LoopbackTransport`. Host dispatch + e2e tests. |
| `server-runtime` | `//drivers/i2c/server-runtime:i2c_server_runtime` | ❌ embedded | The Pigweed WaitGroup wait/respond loop. One channel per bus. |
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
- **No slave/target or interrupt path.** The wire protocol carries only
  `Transaction`; none is invented here.

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

## Slave-RX notification (thin slice)

Client (target side, same bus channel): `configure_slave(addr)` →
`enable_slave()` → `enable_notification()`; then on a `Signals::USER` wake on
the channel, `slave_receive(&mut buf)` (returns `NoData` if nothing latched).
Seam = `openprot_hal_blocking::i2c_hardware::slave` (`I2cSlaveCore`/`Buffer`),
reused after correcting its `I2cHardwareCore` supertrait to `ErrorType`.
Deferred: slave TX/`ReadRequest`, multi-message queue, blocking/timeout, the
`system.json5`/app + MCTP-consumer integration. The IRQ→USER path is
QEMU-verified only (by decision); host tests cover the wire codec + dispatch.

## Test matrix

| Test target | Tags | What it covers |
|-------------|------|----------------|
| `//drivers/i2c/api:i2c_api_test` | `host` | Wire-codec unit tests: `I2cRequestHeader`/`I2cResponseHeader` round-trips, `I2cOpDesc` encode/decode, error + opcode byte mapping stability, slave opcode round-trips (`ConfigureSlave`…`SlaveReceive`), `SlaveReceive` max-len in `op_count`, `NoData` status round-trip |
| `//drivers/i2c/server:i2c_server_test` | `host` | `dispatch()`: write+read round-trip through a mock bus, bus error → wire error-code mapping, short/malformed request rejected without panic. `dispatch_slave()`: configure/enable/disable apply to device, runtime-owned ops (`SlaveReceive`) and malformed requests rejected |
| `//drivers/i2c/tests:i2c_loopback_test` | `host` | End-to-end: consumer drives `I2cClient` purely through the `embedded_hal::i2c::I2c` seam; `LoopbackTransport` routes the **real** client encoders/decoders into `i2c_server::dispatch` onto an `EchoBus` mock. Verifies address, write payload, op ordering, and read scatter — the exact marshalling path used in production |
| `//target/ast10x0/tests/peripherals/i2c/i2c_init:i2c` | `embedded` `qemu` | Full kernel/ARM system image build + boot: `Ast10x0Board::init()` brings up all wired I2C buses (incl. DMA); server-runtime loop starts; slave/notification IRQ path exercised |

**Not yet covered:**
- Slave TX / `ReadRequest` / `slave_set_response` (deferred)
- Multi-message queue + blocking `wait_for_messages` + timeouts (deferred)
- Hardware EVB (`--config=k_ast1060_evb`) — I2C is QEMU-verified only by current decision
- Host-modeled IRQ — notification path is QEMU-only by decision

## Status

Host tests (`bazel test`, no kernel/QEMU): `//drivers/i2c/api:i2c_api_test`
(wire codec incl. slave ops), `//drivers/i2c/server:i2c_server_test`
(`dispatch` + `dispatch_slave` + error map),
`//drivers/i2c/tests:i2c_loopback_test` (end-to-end client↔server marshalling).
Full kernel/ARM stack incl. the slave/notification path and the `i2c_init:i2c`
system image build under `--config=virt_ast10x0`; the `ast1060_pac`
instance-name assumption is **confirmed** (crates compiled clean).
