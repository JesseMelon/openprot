# Assessment: `openprot-i2c` client vs. the `ocp-emea-demo-stack-facade` template

**Date:** 2026-05-18
**Question:** Is `openprot-ocp-emea-demo-stack-facade/` a better template for the i2c
driver? The client there talks to a server that manages several buses.

**Short answer:** Yes — the *structure* (layered crate split + server + backend +
the **Stack facade** pattern + host-testable transport + first-class multi-bus)
is the better template and we should adopt it. **But** the template's i2c client
has one real correctness regression — it has dropped the whole-transaction
atomicity that `openprot-i2c` currently guarantees. Adopt the shape; carry our
atomic-transaction marshalling into it. Do not copy the template's i2c client
verbatim.

---

## 1. What each side actually is

| | `openprot-i2c` (current) | `stack-facade` (template) |
|---|---|---|
| Crates | `api`, `client` | `api`, `client`, `server`, `backend-aspeed` (+ `services/mctp` Stack facade) |
| Server in-tree | **No** | **Yes** — real dispatch loop |
| Seam exposed to consumers | `embedded_hal::i2c::I2c` **verbatim** | bespoke `I2cClient` / `I2cClientBlocking` / `I2cTargetClient` traits (embedded-hal *error* compat only) |
| Multi-bus | No — one handle ⇒ one (implicit) bus | **Yes** — `BusIndex` arg on every call; server owns 14 buses |
| Transaction atomicity | **One seam call ⇒ one round-trip ⇒ one server run-to-completion** | **Broken**: `transaction()` loops per-op = N round-trips (explicitly marked "For now") |
| Slave/target mode | No | Yes (`I2cTargetClient`, interrupt-driven RX) |
| Host testability | No (depends on `userspace` unconditionally) | Yes (`#[cfg(feature = "pigweed")]` + stub; `RefCell` "stateless from caller" pattern) |
| Client size | ~168 LOC, no interior mutability | ~350 LOC, `RefCell` buffers |

---

## 2. Where the template is genuinely better (adopt these)

1. **Full layered split with a real server + backend.** This is the
   `userspace-driver-client` pattern fully realized: `api` (types+traits, no OS),
   `client` (IPC impl), `server` (dispatch loop), `backend-aspeed` (hardware).
   `openprot-i2c` has no server at all — we'd be inventing it anyway.

2. **First-class multi-bus.** `BusIndex` is a real type threaded through the API;
   the server keeps per-bus state (`notification_enabled[14]`) and one dispatch
   loop fans out to all buses. The current design's "one handle = one bus"
   forces a separate server instance/channel per bus. The user's framing — *"a
   server managing several buses"* — is exactly this, and the template nails it.

3. **The Stack facade pattern** (from `services/mctp/api/src/stack.rs`). This is
   the part worth stealing wholesale conceptually:
   - A low-level, platform-independent `MctpClient` *trait*.
   - A `Stack<C: MctpClient>` *facade* that wraps any client and hands back
     **typed RAII channel objects** (`StackReqChannel`, `StackListener`,
     `StackRespChannel`) that implement abstract traits and `Drop` to release
     server handles automatically.
   - The app depends only on the abstract traits — both the stack impl *and* the
     OS transport are invisible and swappable.

4. **Host testability built in.** `IpcMctpClient`/`IpcI2cClient` compile under
   plain Cargo via `#[cfg(not(feature = "pigweed"))]` stubs, and use `RefCell`
   so trait methods stay `&self` ("logically stateless from the caller"). This
   is what lets the protocol be unit-tested off-target — `openprot-i2c` cannot.

5. **Real server concurrency story.** `WaitGroup` multiplexes IPC + a hardware
   IRQ (`user_data` discriminator), interrupt-driven slave RX drains into flat
   buffers, `raise_peer_user_signal` wakes the client. That's a complete
   reference for how the server loop should look.

6. **Convenience layer done right.** `I2cClientBlocking` is a blanket-impl
   extension trait (`write`/`read`/`read_register`/`write_register`/`probe`)
   over the core trait — zero cost, no duplication.

---

## 3. Where the template is worse (do NOT copy these)

1. **Atomicity regression — the important one.** `openprot-i2c` marshals *one
   whole `I2c::transaction`* (address + ordered op list) into a single
   round-trip the server replays atomically with no bus lock/unlock crossing the
   boundary. That is precisely what preserves the `embedded_hal::i2c::I2c`
   exclusive-atomic contract between processes.

   The template's `IpcI2cClient::transaction()` instead **loops over operations
   issuing one `write_read` per op** — multiple round-trips, bus released between
   them, repeated-START semantics lost. The code itself admits it:
   `// For now, handle simple cases by converting to write_read`. And the
   server's `I2cOp::Transaction` arm is stubbed: returns `ServerError`
   ("Not yet implemented"). So the template *has the multi-bus story but lost the
   transaction story* — which is exactly the part `openprot-i2c` got right.

2. **Bespoke trait instead of the embedded-hal seam.** `openprot-i2c` exposes
   `embedded_hal::i2c::I2c` *verbatim*, so any ecosystem device driver works
   unmodified and the server backend can be any real `I2c` impl. The template
   invents `I2cClient` with a `BusIndex` parameter on every method — *not*
   drop-in embedded-hal (it only borrows the `ErrorType`/`Error` taxonomy).
   This is a deliberate trade: **bus multiplexing vs. ecosystem compatibility.**
   embedded-hal models "a bus" as a handle, not an argument; multi-bus is
   normally N client instances, not a bus arg.

3. **More surface, more state.** `RefCell`, larger client, target-mode API
   intermixed. Fine, but it is not free; the current client is trivially
   auditable.

---

## 4. Recommendation

**Adopt the template's architecture; port our transaction discipline into it.**

Concretely, the target design:

1. **Crate layout:** `api` / `client` / `server` / `backend-<soc>` as in
   `services/i2c`. Add a `Stack`-style facade crate/module modeled on
   `services/mctp/api/src/stack.rs`.

2. **Keep the whole-transaction marshalling from `openprot-i2c`.** Implement the
   stubbed `I2cOp::Transaction` in the server (descriptor array + inline write
   payloads in, scatter reads back, one run-to-completion). This closes the
   exact gap the template left open. Reuse `drivers/i2c/api/src/protocol.rs`
   nearly as-is — it already defines this wire format.

3. **Seam — RESOLVED (2026-05-18).** A client process is bound to exactly **one**
   bus by configuration (the capability/handle it is granted). The bus is
   therefore *never* an API argument. The client stays a plain
   `embedded_hal::i2c::I2c` object **identical in shape to today's
   `openprot-i2c` client** — ecosystem drivers work unmodified. **No `BusIndex`
   in the client API.** All multi-bus logic lives in the server only.

   Server-side routing — **DECIDED (2026-05-18): one IPC channel per bus.**
   The server `WaitGroup` watches one client channel per bus (plus the IRQ
   source). The channel a request arrives on *is* the bus — no bus id on the
   wire, strongest isolation (a client physically cannot address another bus).
   The wire protocol (`drivers/i2c/api/src/protocol.rs`) is unchanged.

4. **Backend wires to `target/ast10x0/peripherals/i2c`.** The per-SoC backend
   crate is a thin adapter over the existing ast10x0 driver — it does **not**
   reimplement i2c. Concretely it drives
   [`Ast1060I2c`](target/ast10x0/peripherals/i2c/controller.rs) (re-exported from
   `target/ast10x0/peripherals/i2c/mod.rs`), which **already implements
   `embedded_hal::i2c::I2c<SevenBitAddress>`** at
   [`hal_impl.rs:151`](target/ast10x0/peripherals/i2c/hal_impl.rs#L151). Because
   the seam *is* `embedded_hal::i2c::I2c`, the server feeds a decoded wire
   transaction straight into `Ast1060I2c` — **no typestate-impedance shim**.
   - The server holds **one `Ast1060I2c` instance per bus it owns**, calls
     `init_i2c_global()` once at startup and per-bus controller init, and
     routes each channel's requests to that bus's instance.
   - Slave/target mode reuses the driver's existing `SlaveBuffer` /
     `SlaveConfig` / `SlaveEvent` types and `error::I2cError` from the same
     module — mapped onto the wire `I2cError` via `seam::error_kind`.

5. **Keep host-testability** (`#[cfg(feature = "pigweed")]` + stub, `RefCell` so
   the facade's methods stay `&self`) — copy this verbatim from the template.

6. **Keep the embedded-hal error taxonomy mapping** (both sides already do this:
   `seam::error_kind` here, `response_to_error` there).

---

## 4b. Execution status (2026-05-18)

Implemented in `drivers/i2c/`, mirroring `drivers/usart` conventions (not the
stack-facade `app_package` wiring), adapting rather than copying:

- **`api` / `client`: untouched** — the atomic whole-transaction seam was
  already correct; kept verbatim.
- **`server` (new):** pure generic `dispatch()` (Transaction-only, no
  panic on malformed input, host-unit-tested with a mock `I2c`) + a
  topology-agnostic `runtime::run` implementing **one IPC channel per bus**.
  No `park`/pending/IRQ/slave machinery — none applies to i2c.
- **`target/ast10x0/backend/i2c` (new):** thin adapter — bus→register-pointer
  map, `init_bus` (per-controller bring-up), and no-init open wrappers
  (`open_bus` / `open_bus_dma`) over the in-tree `Ast1060I2c` (already
  `embedded_hal::i2c::I2c`); no typestate shim. Lives under `target/` (not
  `drivers/i2c/`) because it is the only crate that names silicon — mirrors
  `target/ast10x0/backend/usart`. Server stays decoupled from it (errors
  cross via the embedded-hal `ErrorKind` taxonomy).
- **Init split (revised by decision):** per-bus `I2cConfig` moved into
  `Ast10x0BoardDescriptor.i2c_buses`; `Ast10x0Board::init()` eagerly brings
  up *every* wired controller (incl. DMA) and returns `Result`. Server opens
  buses via the no-init `from_initialized*` path. DMA buses use `open_bus_dma`
  with a server-owned `.ram_nc` buffer (the single documented exception —
  a `&'static` descriptor cannot carry a unique mutable DMA buffer).
- **Not done (deliberately, no precedent to copy):** a server *binary* and
  per-bus `system.json5`/`app_package` — deferred to a concrete board target
  rather than fabricated. Bazel build unverified in the sandbox; one PAC
  type-name assumption flagged in `drivers/i2c/README.md`.

## 5. One-line verdict

The `stack-facade` clone is the better **structural** template (layering,
server, multi-bus, the Stack facade, host-testability). `openprot-i2c`'s one
asset the template lacks — **atomic whole-transaction marshalling** — must be
carried forward, not discarded. Adopt the shape, keep our transaction.
