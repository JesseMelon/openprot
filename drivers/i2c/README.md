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
| `client-ipc` | `//drivers/i2c/client-ipc:i2c_client_ipc` | ❌ kernel | `IpcTransport` (`channel_transact`). The one IPC-coupled client piece. |
| `server` | `//drivers/i2c/server:i2c_server` | ✅ | Pure `dispatch()` + `LoopbackTransport`. Host dispatch + e2e tests. |
| `server-runtime` | `//drivers/i2c/server-runtime:i2c_server_runtime` | ❌ kernel | The Pigweed WaitGroup wait/respond loop. One channel per bus. |
| `backend` (ast10x0) | `//target/ast10x0/backend/i2c:i2c_backend_ast10x0` (crate `i2c_backend`) | ❌ kernel | bus → reg-ptr map, `init_bus`, `open_bus`/`open_bus_dma`. Under `target/`. |

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
4. `i2c_server_runtime::run(handle::WG, buses)`.

The matching `system.json5` declares one `channel_handler` per bus (plus one
`wait_group`). Deferred until a concrete board target needs it — not
fabricated speculatively.

## Status

Host tests (`bazel test`, no kernel/QEMU): `//drivers/i2c/api:i2c_api_test`
(wire codec), `//drivers/i2c/server:i2c_server_test` (dispatch + error map),
`//drivers/i2c/tests:i2c_loopback_test` (end-to-end client↔server marshalling).
Kernel/ARM crates build under `--config=virt_ast10x0`; the `ast1060_pac`
instance-name assumption (`I2c`/`I2c1..13`, `I2cbuff`/`I2cbuff1..13`) is
**confirmed** — `i2c_backend_ast10x0` + `ast10x0_board` compiled clean.
