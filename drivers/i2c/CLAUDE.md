<!-- Licensed under the Apache-2.0 license -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# i2c driver — agent context

Auto-loaded when working under `drivers/i2c`. This is the portable
reconstruction of the working memory for this effort (the `~/.claude` memory
is machine-local and does not travel). Human-facing rationale:
`drivers/i2c/README.md` and `ASSESSMENT-stack-facade-template.md` (repo root).

## Status — NOT complete

Master-only milestone (commit "i2c: layered userspace driver"). The
fully-functional **ocp-emea demo still needs i2c slave/target behavior +
notifications** brought in. Currently scoped out on purpose: the wire
protocol (`api/src/protocol.rs`) carries only `Transaction`, no slave ops, no
IRQ. Slave RX + notification — cf. the `ocp-emea-demo-stack-facade` clone's
`I2C2_IRQ` / `drain_slave_rx` / `raise_peer_user_signal` — is the next chunk
of real work, not a deferred nicety.

## Locked decisions (do not re-litigate)

- Consumer seam = `embedded_hal::i2c::I2c` **verbatim**. No `BusIndex`.
- **One IPC channel per bus.** Multi-bus lives only in the server; a client
  is wired by config to one bus and cannot name another.
- Per-bus `I2cConfig` lives in `Ast10x0BoardDescriptor`. `Ast10x0Board::init()`
  eagerly brings up *every* wired controller (incl. DMA) and returns `Result`.
  Server opens buses via the no-init `from_initialized*` path (`open_bus` /
  `open_bus_dma`); `new()` is not used at server start.
- DMA buses: server binary owns the `#[link_section=".ram_nc"]` buffer,
  passed to `open_bus_dma` (a `&'static` descriptor can't carry it).
- SoC backend lives under `target/ast10x0/backend/i2c` (only crate that names
  silicon), NOT in `drivers/i2c/`.

## Crate map (host = `bazel test`, kernel = `--config=virt_ast10x0`)

| Crate | Host? | Role |
|---|---|---|
| `api` | host | wire protocol + `I2c` seam + `Transport` seam |
| `client` | host | `I2cClient<T: Transport>`; all marshalling; surface = `new()` + seam |
| `client-ipc` | kernel | `IpcTransport` (`channel_transact`) — only IPC piece |
| `server` | host | pure `dispatch()` + `LoopbackTransport` |
| `server-runtime` | kernel | WaitGroup wait/respond loop, `Bus`, `run()` |
| `target/ast10x0/backend/i2c` | kernel | `init_bus`/`open_bus`/`open_bus_dma` |

## Build / test (bazelisk at ~/.local/bin)

```
# host tests, no kernel/QEMU:
bazelisk test //drivers/i2c/api:i2c_api_test \
               //drivers/i2c/server:i2c_server_test \
               //drivers/i2c/tests:i2c_loopback_test
# kernel/ARM crates + system image:
bazelisk build --config=virt_ast10x0 //drivers/i2c/... \
   //target/ast10x0/backend/i2c:i2c_backend_ast10x0 \
   //target/ast10x0/board:ast10x0_board \
   //target/ast10x0/tests/peripherals/i2c/i2c_init:i2c
```
All host tests pass; all kernel crates + `i2c_init:i2c` image build (verified
2026-05-18). Kernel libs are `tags=["kernel"]` + `TARGET_COMPATIBLE_WITH` and
are incompatible with the host platform — build them with the config above,
don't report that as a failure.

## Working guardrails (learned the hard way here)

- **Adapt, don't wholesale-copy** the usart/stack-facade references. For each
  lifted construct, state the i2c problem it solves; if none, drop it. (The
  usart `park`/`PendingRead` was rejected — i2c is strict run-to-completion.)
- **"Leave X unchanged" is not a virtue.** Check every such decision against
  the template invariants — especially host-testable protocol (Transport
  seam + loopback), the platform-agnostic boundary, and atomicity. Preserving
  a pre-template crate verbatim once silently dropped host-testability.
- **Verify by building.** bazelisk works here; run the commands above and
  report real pass/fail, not assumptions.

## Invariants that must keep holding

One client `I2c::transaction` ⇒ one `Transport::transact` ⇒ one server-side
run-to-completion ⇒ one reply (never fragment per-op). Server generic over
`embedded_hal::i2c::I2c`, never depends on the SoC backend. Same client
encode/decode exercised on host (loopback) and in production (IPC) — no fork.
