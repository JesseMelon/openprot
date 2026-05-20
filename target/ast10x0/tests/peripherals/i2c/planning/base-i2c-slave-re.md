# Reverse Engineering: `base/target/ast1060-evb/i2c-slave`

## 1. System Composition

Three binaries packaged into one `system_image`:

| Component | Bazel target | Role |
|-----------|-------------|------|
| Kernel | `:target` (target.rs) | Boots, starts apps, no I2C logic |
| `i2c_server` | `//services/i2c/server:i2c_server` | Userspace IPC server; owns hardware |
| `i2c_slave_echo` | `//apps/i2c-slave-echo:i2c_slave_echo` | Userspace app; register-model echo |

Kernel `target.rs` is a thin shell: calls `codegen::start()` then loops
forever. All real logic is in the two apps.

## 2. Memory Layout (system.json5)

```
0x00000000 – 0x0000067F  Vector table + kernel annotations (1664 bytes)
0x00000680 – 0x0001FFFF  Kernel flash  (~126 KB, 129408 bytes)
0x00020000 – 0x0003FFFF  i2c_server flash (128 KB)
0x00040000 – 0x0004FFFF  i2c_slave_echo flash (64 KB)
0x00060000 – 0x0007FFFF  Kernel RAM (128 KB)
0x00080000 – 0x0008FFFF  i2c_server RAM (64 KB)
0x00090000 – 0x00097FFF  i2c_slave_echo RAM (32 KB)
```

## 3. IPC Topology (system.json5)

```
i2c_slave_echo                   i2c_server
┌──────────────────┐             ┌──────────────────────┐
│ channel_initiator│ ──"I2C"──▶  │ channel_handler "I2C"│
│ (I2C object)     │             │                      │
└──────────────────┘             └──────────────────────┘
```

- `i2c_slave_echo` holds a `channel_initiator` for `I2C` bound to
  `i2c_server`'s `channel_handler`.
- No IRQ or WaitGroup objects are currently wired (all commented out in
  system.json5 and server.rs) — the server runs in blocking-poll mode.

## 4. `i2c_server` Architecture

**Entry / dispatch loop** (`services/i2c/server/src/main.rs`):

```
init: AspeedI2cBackend::new() → init_bus(2)
loop:
  object_wait(handle::I2C, READABLE)
  channel_read → I2cRequestHeader
  dispatch_i2c_op(op, backend, ...)
  channel_respond ← I2cResponseHeader [+ payload]
```

**Dispatch table** (all ops handled):

| `I2cOp` | Backend call | Notes |
|---------|-------------|-------|
| `Write` | `backend.write(bus, addr, data)` | |
| `Read` | `backend.read(bus, addr, buf)` | payload appended after header |
| `WriteRead` | `backend.write_read(...)` | repeated-START |
| `Probe` | `backend.write(bus, addr, &[])` | 0-byte write |
| `RecoverBus` | `backend.recover_bus(bus)` | |
| `ConfigureSlave` | `backend.configure_slave(bus, addr)` | |
| `EnableSlave` | `backend.enable_slave(bus)` | |
| `DisableSlave` | `backend.disable_slave(bus)` | |
| `SlaveReceive` | `backend.slave_receive(bus, buf)` OR `get_buffered_slave_message` | blocking vs IRQ-driven |
| `SlaveSetResponse` | `backend.slave_set_response(bus, data)` | pre-loads TX buffer |
| `SlaveWaitEvent` | `backend.slave_wait_event(bus, rx_buf)` | returns `(SlaveEventKind, rx_len)` |
| `EnableSlaveNotification` | `backend.enable_slave_notification(bus)` | sets `notification_enabled[bus]` |
| `DisableSlaveNotification` | `backend.disable_slave_notification(bus)` | |
| `ConfigureSpeed`, `Transaction` | ❌ `ServerError` | not implemented |

**IRQ path (dead code, fully commented out):**
- WaitGroup would multiplex IPC (`user_data=0`) and I2C2 IRQ (`user_data=1`).
- IRQ handler: iterate buses → `drain_slave_rx(bus)` → `interrupt_ack` →
  `object_raise_peer_user_signal(I2C)`.
- Currently disabled: server uses `object_wait(I2C, READABLE)` directly
  (blocking poll only).

**`SlaveReceive` dual path:**
- If `notification_enabled[bus]` → `get_buffered_slave_message` (non-blocking,
  data pre-drained by IRQ handler).
- Else → `slave_receive(bus, buf)` (blocking spin until `DataReceived` or
  timeout; timeout returns 0 bytes, not an error).

## 5. `i2c_slave_echo` Application

**Protocol** (I2C2, address `0x42`):

```
Master write [reg, val]  →  reg_map[reg] = val
                             slave_set_response([val])   ← pre-load read ptr
                             log: "WRITE reg=0xRR val=0xVV"

Master write [reg]       →  reg_ptr = reg
                             slave_set_response([reg_map[reg]])
                             log: "READ  reg=0xRR val=0xVV"

Master read N bytes      →  returns pre-loaded TX buffer (set above)
                             SlaveEventKind::ReadRequest received (no extra log)

Master write [] (0 bytes) →  probe / no-op (ignored)
```

**Event loop:**
```
client.configure_target_address(BUS_2, 0x42)
client.enable_receive(BUS_2)
client.slave_set_response(BUS_2, &[reg_map[0]])  // initial preload

loop:
  client.slave_wait_event(BUS_2, &mut rx_buf) →
    DataReceived(n=0)     → ignore
    DataReceived(n=1)     → update reg_ptr; preload read response
    DataReceived(n>=2)    → write reg_map[rx[0]] = rx[1]; preload
    ReadRequest           → master consumed TX buffer (no action needed)
    Stop                  → no-op
    Err                   → continue
```

**State:** 256-byte `reg_map` (all zeros on boot). `reg_ptr` tracks last
register address set by a single-byte write.

## 6. Wire Format

**Request header** (`I2cRequestHeader`):
- Fixed size (`I2cRequestHeader::SIZE`)
- Fields: `bus: u8`, `address: u8`, `op: I2cOp`, `write_len: u16`,
  `read_len: u16`
- Payload (write data) follows immediately after header

**Response header** (`I2cResponseHeader`):
- Fixed size (`I2cResponseHeader::SIZE`)
- Fields: `code: ResponseCode`, `data_len: u16`
- Read/SlaveReceive/SlaveWaitEvent data appended after header

**`SlaveWaitEvent` response payload:**
```
byte 0:       SlaveEventKind as u8
bytes 1..:    received data (DataReceived only)
```

## 7. Key Constraints and Observations

1. **No IRQ wiring today** — all slave event delivery is blocking-poll via
   `slave_wait_event`. The WaitGroup + IRQ skeleton exists in both system.json5
   (commented out) and server.rs but is not active.

2. **Bus 2 only** — `init_bus(2)` is the only bus brought up. Comment says
   "TODO: Initialize all buses this server owns."

3. **`slave_set_response` timing** — the echo app pre-loads the TX buffer
   *during the DataReceived handler* for the preceding write, so it's ready
   before the master issues a Read. This is the standard read-after-write
   latency management pattern.

4. **`SlaveWaitEvent` is the primary slave event API** — supersedes the older
   `SlaveReceive` polling loop. The echo app uses only `SlaveWaitEvent`.

5. **`ReadRequest` is a no-op in the echo app** — the response was pre-loaded;
   the event just confirms the master consumed it. The commented-out log shows
   this was debated.

6. **`notification_enabled` array** — per-bus boolean, 14 elements. Only
   relevant when IRQ path is re-enabled.

7. **`uart_boot_image` / `uart_upload`** — BUILD.bazel generates a UART
   bootable binary and an `upload_i2c_slave` run target
   (`bazel run :upload_i2c_slave --config=k_ast1060_evb`).
