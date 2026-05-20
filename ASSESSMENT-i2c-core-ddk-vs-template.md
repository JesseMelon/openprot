# Assessment: `aspeed-rust` i2c_core (DDK) vs. `i2c-driver-template` peripherals/i2c

**Date:** 2026-05-19  
**Scope:** File-by-file comparison of `aspeed-rust/src/i2c_core/` (DDK reference)
against `i2c-driver-template/target/ast10x0/peripherals/i2c/` (this repo).

---

## Overall Scores

Scoring 1–5 (higher = better) across six dimensions.

| Dimension | DDK | Template |
|---|:---:|:---:|
| **Unsafe perimeter** — how contained is `unsafe` register access | 2 | 5 |
| **Correctness — `write_read`** — does it produce a repeated-START? | 2 | 4 |
| **Correctness — slave TX** — does multi-byte TX arm correctly? | 2 | 4 |
| **Correctness — master/slave coexistence** — TX_ACK stuck errata | 1 | 5 |
| **Lifetime / ownership clarity** — struct lifetimes and DMA ownership | 2 | 4 |
| **Extensibility / yield** — can the caller control spin behaviour? | 2 | 5 |
| **Total** | **11 / 30** | **27 / 30** |

---

## 1. Architecture Overview

### DDK (`aspeed-rust/src/i2c_core/`)

The DDK holds MMIO pointers through a lifetime-bearing descriptor struct
`I2cController<'a>` that contains `&'a RegisterBlock` and
`&'a BuffRegisterBlock`. Every `Ast1060I2c<'a>` borrows that descriptor.
Register access is scattered: any module that holds `&self` can dereference
`self.controller.registers` directly.

```
I2cController<'a> { &'a RegisterBlock, &'a BuffRegisterBlock }
              ↑
Ast1060I2c<'a>  { controller: &'a I2cController<'a>, dma_buf: Option<&'a mut [u8]>, … }
```

**Unsafe perimeter**: the raw pointer → reference cast happens at every
access site through the lifetime-erased `&'a RegisterBlock` reference.
There is no single `unsafe` gate; the safety contract is distributed.

**Yield**: the DDK imports `DummyDelay` from `crate::common` — a fixed
busy-wait with no caller hook. There is no way to substitute a scheduler
yield or an RTOS sleep without modifying the crate.

**DMA**: a single `dma_buf: Option<&'a mut [u8]>` field is used for both
master TX/RX staging and slave RX. Master and slave operations share the
same buffer.

### Template (`target/ast10x0/peripherals/i2c/`)

The template introduces the *Confined-unsafe MMIO Façade* pattern.
`Ast1060I2cRegisters` is a `Copy` struct that holds `*mut` pointers and
exposes `pub(crate) fn i2c(&self) -> &RegisterBlock` / `buff()`.
The **only** `unsafe` block in the entire driver is inside
`Ast1060I2cRegisters::new()` — a single `unsafe const fn` discharged once
by the caller at construction time.

```
Ast1060I2cRegisters { *mut RegisterBlock, *mut BuffRegisterBlock }  // Copy; unsafe confined here
                  ↑
Ast1060I2c<'a, Y> { mmio: Ast1060I2cRegisters, master_dma_buf, slave_dma_buf, yield_ns: Y }
```

`Y: FnMut(u32)` gives the caller full control over polling behaviour.
`master_dma_buf` and `slave_dma_buf` are separate fields.

---

## 2. File-by-File Changes

### 2.1 `registers.rs` — **new file, no DDK equivalent**

| | DDK | Template |
|---|---|---|
| MMIO pointer storage | `&'a RegisterBlock` in `I2cController<'a>` | `*mut RegisterBlock` in `Ast1060I2cRegisters` |
| `unsafe` perimeter | Distributed: any `&self.controller.registers` deref | Confined: one `unsafe const fn new()` + two `pub(crate)` safe accessors |
| `Copy` | No (contains lifetime reference) | Yes — enables transient driver construction per operation |
| Aliasing discipline | Enforced by borrow checker via lifetime | Enforced by single-owner contract documented at `new()` |

**Why it matters**: the template's `Ast1060I2cRegisters` being `Copy` is the
key enabler for `Ast1060I2cBackend::make_driver()` — constructing a fresh
`Ast1060I2c` per HAL call without re-doing hardware init. The DDK pattern
cannot do this because `I2cController<'a>` is lifetime-bearing and cannot
be freely copied.

**Score**: DDK 2/5 · Template 5/5

---

### 2.2 `controller.rs` — **structural rewrite, no logic change**

| | DDK | Template |
|---|---|---|
| MMIO field | `controller: &'a I2cController<'a>` | `mmio: Ast1060I2cRegisters` (owned, Copy) |
| Delay / yield | `DummyDelay` from `crate::common` — hard-coded busy-wait | `yield_ns: Y` — caller-supplied `FnMut(u32)` |
| DMA buffer(s) | `dma_buf: Option<&'a mut [u8]>` — single shared buffer | `master_dma_buf` + `slave_dma_buf` — separate fields |
| Logic changes | — | None; hardware init sequence identical |

**Why `yield_ns: Y` matters**: the DDK's `wait_completion` spins on a
fixed `DummyDelay`. In the template the server passes `spin as fn(u32)` for
busy-wait, but any RTOS-aware caller can pass a task-yield closure. This
makes the driver portable to cooperative schedulers without a code change.

**Why split DMA matters**: the DDK's single `dma_buf` is used for both
master TX staging and slave RX. When master and slave are active on the same
bus (DMA mode), they race on the same buffer. The template separates them so
a large master buffer (≤4096 B) and a small slave buffer (256 B) are
independent.

**Score**: DDK 2/5 · Template 5/5

---

### 2.3 `master.rs` — **three functional changes**

#### Change 1: `stop: bool` parameter on write primitives

DDK signature:
```rust
fn write_byte_mode(&mut self, addr: u8, bytes: &[u8]) -> Result<(), I2cError>
fn write_buffer_mode(&mut self, addr: u8, bytes: &[u8]) -> Result<(), I2cError>
fn write_dma_mode(&mut self, addr: u8, bytes: &[u8]) -> Result<(), I2cError>
```

Template signature:
```rust
fn write_byte_mode(&mut self, addr: u8, bytes: &[u8], stop: bool) -> Result<(), I2cError>
fn write_buffer_mode(&mut self, addr: u8, bytes: &[u8], stop: bool) -> Result<(), I2cError>
fn write_dma_mode(&mut self, addr: u8, bytes: &[u8], stop: bool) -> Result<(), I2cError>
```

The DDK always appends `STOP_CMD` at the end of every write phase.
The template makes the STOP conditional: `if is_last && stop { cmd |= STOP_CMD }`.
When `stop = false` the hardware holds SCL low (clock-stretch) after the last
TX ACK, keeping the bus owned by this master without issuing a STOP.

This is an internal change; the public `write()` method always passes
`stop = true` so external callers are unaffected.

#### Change 2: `write_read` — repeated-START vs. STOP+START

| | DDK | Template |
|---|---|---|
| Protocol | `self.write(addr, bytes)?;  self.read(addr, buffer)?;` — two separate transactions | Write with `stop=false`, then read — one atomic transaction |
| Bus state between phases | STOP issued; bus released; arbitration possible | Bus held (clock-stretch); no STOP; repeated-START semantics |
| MCTP/SMBus correctness | **Wrong** — two transactions; another master can steal the bus; some devices treat stop+start as a new transaction | **Correct** — matches MCTP write-then-read semantics |

##### Origin of the template's approach

**Reference: Zephyr `drivers/i2c/i2c_aspeed.c`, tag `v00.03.06`
(`AspeedTech-BMC/zephyr`)**

Zephyr implements `write_read` as a true repeated-START via a multi-message
ISR state machine in `aspeed_i2c_master_irq()`. When the driver processes
a `msgs[]` array and transitions from a write message to a read message, it
issues `START_CMD` on the read without a prior `STOP_CMD`. The hardware sees
a START on a still-held bus and produces a repeated-START (Sr) on the wire.
No STOP is ever placed between the two phases.

The DDK's `write_read` calls `self.write()` then `self.read()` — two
independent paths, each issuing a full STOP. This is a regression relative
to the Zephyr reference.

**Polling-model adaptation (clock-stretch)**

Zephyr's ISR approach requires `msgs[]` to persist across interrupt calls —
it is fundamentally asynchronous. The template is synchronous (polling).
The adaptation relies on one AST1060 hardware property: if `STOP_CMD` is
withheld, the hardware holds SCL low (clock-stretch) after the last TX ACK.
The CPU then has a window — microseconds — to issue the read command before
the bus is auto-released. `START_CMD` on a held bus is a repeated-START.

```
write_*_mode(addr, bytes, stop=false)  →  SCL held low (clock-stretch)
read_*_mode(addr, buffer)              →  START_CMD on held bus  =  Sr (repeated-START)
```

**Precondition**: `smbus_timeout` must be disabled (or set long enough) in
the `I2cConfig` for this bus. If SMBus timeout fires before the read command
lands, the hardware auto-issues a STOP and the transaction degrades to
STOP+START. The Zephyr ISR state machine does not have this constraint because
the read command is issued from within the interrupt handler — before the
hardware can time out.

**What remains deferred**

A true Zephyr-style implementation would hold a `msgs: &[I2cMsg]` slice
reference across interrupt boundaries and advance through it in
`handle_interrupt()`. This requires non-trivial lifetime work (the slice must
outlive the ISR call chain) and is tracked as a future improvement. The
polling approach is correct for the current single-threaded server model.

**Score for `write_read`**: DDK 2/5 · Template 4/5  
(Template is correct for polling with SMBus timeout disabled; Zephyr ISR
state machine would be 5/5 — deferred work.)

#### Change 3: TX_ACK workaround (silicon errata)

The DDK has no workaround for the master/slave packet-mode coexistence bug.
When master `TX_ACK` fires mid-transaction while slave packet mode is active
(`AST_I2CS_PKT_MODE_EN` set in `i2cs28`), the slave state machine latches a
spurious `RX_DONE` and NACKs the next master byte.

The template adds the following in the `PKT_DONE` ISR branch:

```rust
if status & (AST_I2CM_TX_ACK | AST_I2CM_NORMAL_STOP) == AST_I2CM_TX_ACK {
    if self.regs().i2cs28().read().enbl_slave_pkt_op_mode().bit() {
        let slave_cmd = self.regs().i2cs28().read().bits();
        self.regs().i2cs28().write(|w| unsafe { w.bits(0) });
        self.regs().i2cs28().write(|w| unsafe { w.bits(slave_cmd) });
    }
}
```

This pulse-clears and restores `i2cs28`, resetting the slave state machine
without disturbing the master transaction. This fix is upstream in the
Zephyr `i2c_aspeed.c` driver (tag `v00.03.06`, ~line 1284) and is
field-validated.

Without this fix, any DMA-mode bus running simultaneous master and slave
operations will intermittently NACK mid-transaction.

**Score for coexistence errata**: DDK 1/5 · Template 5/5

---

### 2.4 `slave.rs` — **two register-write changes**

#### Change 1: multi-byte slave TX in buffer mode

DDK:
```rust
let to_write = 1;  // always 1 byte
self.regs().i2cs38().write(|w| unsafe { w.tx_data_byte_count().bits(to_write as u8 - 1) });
cmd |= AST_I2CS_TX_BUFF_EN;
```

Template:
```rust
let to_write = data.len().min(BUFFER_SIZE);
self.regs().i2cs38().write(|w| unsafe { w.tx_data_byte_count().bits((to_write - 1) as u8) });
cmd |= AST_I2CS_TX_BUFF_EN | AST_I2CS_RX_BUFF_EN;
```

The DDK always declares TX count = 1, regardless of how many bytes the
caller provides. This silently truncates any multi-byte slave response.
The template sets the TX count to `data.len()` (clamped to `BUFFER_SIZE`),
matching the semantics the caller expects.

#### Change 2: atomic RX re-arm during slave TX

The DDK only asserts `AST_I2CS_TX_BUFF_EN` when arming a slave transmit.
The template also asserts `AST_I2CS_RX_BUFF_EN` in the same `i2cs28` write.

Without `RX_BUFF_EN` being reasserted, there is a window between issuing the
TX arm and the next explicit RX arm where an incoming master write will be
missed (the slave's RX buffer is not active). By keeping both bits set
atomically, the template eliminates that window.

This mirrors the Zephyr driver's approach of keeping RX always armed during
a packet-mode slave session.

**Score for slave.rs**: DDK 2/5 · Template 4/5  
(Template is correct; a true event-driven ISR state machine would score 5/5.)

---

### 2.5 `timing.rs` and `transfer.rs` — **structural-only**

Both files are functionally identical to the DDK. The only changes are:
- SPDX header line added
- `impl Ast1060I2c<'_>` → `impl<Y: FnMut(u32)> Ast1060I2c<'_, Y>` (type parameter propagation)

No register writes changed. No logic changed.

**Score**: DDK 5/5 · Template 5/5 (tied)

---

### 2.6 `types.rs` — **`I2cController` removed**

The DDK defines:
```rust
pub struct I2cController<'a> {
    pub controller: Controller,
    pub registers: &'a ast1060_pac::i2c::RegisterBlock,
    pub buff_registers: &'a ast1060_pac::i2cbuff::RegisterBlock,
}
```

This is the descriptor that `Ast1060I2c<'a>` borrows. The template removes
it entirely: MMIO is now owned by `Ast1060I2cRegisters` (in `registers.rs`),
making `types.rs` purely configuration types (`I2cConfig`, `I2cSpeed`,
`I2cXferMode`, `ClockConfig`).

Removing `I2cController` eliminates the lifetime coupling between the
descriptor and the driver — a necessary precondition for the
`Ast1060I2cBackend::make_driver()` pattern.

---

## 3. Summary Table

| File | DDK score | Template score | Nature of change |
|---|:---:|:---:|---|
| `registers.rs` (new) | — | 5/5 | New: confined-unsafe MMIO façade; enables `Copy` + `make_driver()` |
| `controller.rs` | 2/5 | 5/5 | Structural: façade ownership, split DMA buffers, `yield_ns: Y` |
| `master.rs` — `stop: bool` | 3/5 | 4/5 | Functional: enables repeated-START path |
| `master.rs` — `write_read` | 2/5 | 4/5 | Correctness fix: STOP+START → repeated-START |
| `master.rs` — TX_ACK workaround | 1/5 | 5/5 | Silicon errata fix: master/slave PKT_DONE coexistence |
| `slave.rs` — multi-byte TX | 2/5 | 4/5 | Correctness fix: always-1-byte → `data.len()` |
| `slave.rs` — atomic RX re-arm | 2/5 | 4/5 | Correctness fix: eliminates RX miss window during TX arm |
| `timing.rs` | 5/5 | 5/5 | Structural only; logic identical |
| `transfer.rs` | 5/5 | 5/5 | Structural only; logic identical |
| `types.rs` | 3/5 | 4/5 | `I2cController` removed; simplifies lifetime graph |

---

## 4. Known Gaps in the Template (vs. Zephyr reference)

| Gap | Status |
|---|---|
| `write_read` true repeated-START via ISR state machine | Deferred — polling model is correct but not interrupt-driven |
| Slave TX: DMA mode multi-byte path | DDK's DMA slave write removed pending validation |
| Bus recovery (`recover_bus`) | Implemented in `I2cBusRecovery` trait; 9-clock bit-bang in `recovery.rs` |
