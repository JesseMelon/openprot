<!-- Licensed under the Apache-2.0 license -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Slave RX Test — Implementation Plan

**Scope:** EVB hardware test — AST1060 as I2C slave on Bus 2, receiving from
an external master. One binary, no IPC — direct driver access through the
existing `Ast1060I2c` backend. Mirrors the `i2c_init` pattern: `target.rs`
constructs `Ast10x0Board` with the bus descriptor, calls `board.init()`, then
obtains the driver via `open_bus()`.

**Tags:** `embedded` `hardware` (EVB only — QEMU does not model I2C slave RX)

---

## 1. Topology

```
  External Master                   AST1060 EVB (Slave)
  ┌─────────────────────┐           ┌─────────────────────┐
  │              SDA    ├───┬───────┤ SDA           I2C2  │
  │              SCL    ├──┬┼───────┤ SCL                 │
  │              GND    ├──┼┼───────┤ GND                 │
  └─────────────────────┘  ││       └─────────────────────┘
    (AST2600, another     ┌┴┴┐
     EVB, or bus master)  │Rp│ Pull-ups (4.7kΩ each)
                          └┬┬┘
                           VCC
```

The AST1060 is the **slave only** on this bus. An external master (AST2600 or
another EVB) initiates all transactions. There is no self-addressed loopback —
the `aspeed-rust` EVB tests (`i2c_master_slave_test.rs`) confirm this topology.

The slave binary starts first, arms the slave address, then waits. The
external master sends the test payload. The binary drains and asserts.

---

## 2. Test binary location

```
target/ast10x0/tests/peripherals/i2c/i2c_slave_rx/
    BUILD.bazel
    system.json5        (copy/adapt from i2c_init)
    target.rs
```

Mirror the `i2c_init` test structure: `system_image` + `system_image_test`
with `tags = ["hardware"]` and `target_compatible_with` excluding QEMU.

---

## 3. Step-by-step test flow

```
Phase A — board init  (in target.rs, mirrors i2c_init)
──────────────────────────────────────────────────────
A1. Ast10x0Board::new(Ast10x0BoardDescriptor {
        pinctrl_groups: &[PINCTRL_I2C2],
        i2c_buses: &[(BUS_2, &SLAVE_CFG)],
    })
A2. unsafe { board.init() }           SCU clock/reset + pin-mux + init_bus(2)

Phase B — configure slave and wait
────────────────────────────────────
B1. board.open_bus(BUS_2, &SLAVE_CFG) → driver: Ast1060I2c
B2. driver.configure_slave_address(0x50)
B3. driver.enable_slave_mode()
B4. [print "SLAVE READY — start external master now"]

Phase C — poll for data from external master
─────────────────────────────────────────────
C1. loop: driver.poll_slave_data()
    → spins until Ok(Some(n)) (master sent data)

Phase D — drain + assert
──────────────────────────
D1. driver.read_slave_buffer(&mut rx[..n])
    → expect Ok(n)
D2. assert rx[..n] == expected_payload   (agreed with external master)
D3. [print pass/fail + received bytes]

Phase E — cleanup
───────────────────
E1. poll_slave_data() → Ok(None)    latch empty after drain
E2. driver.disable_slave_mode()
```

**External master script** (run after "SLAVE READY" appears on UART):
```
transaction(0x50, [Write(&[0xDE, 0xAD, 0xBE, 0xEF])])
```
Agree the payload constant between the slave binary and whoever drives the master.

---

## 4. `I2cConfig` for the test bus

Use `BufferMode` (no DMA) at `Standard` speed, matching the `aspeed-rust`
EVB slave test. I2C2 (`PINCTRL_I2C2`) is the bus used in the existing
`i2c_master_slave_test.rs` for slave mode.

```rust
const SLAVE_CFG: I2cConfig = I2cConfig {
    speed: I2cSpeed::Standard,
    xfer_mode: I2cXferMode::BufferMode,
    smbus_timeout: false,
    clock: ClockConfig::ast1060_default(),
};
```

---

## 5. Assertion strategy

Use the same `console_backend_write_all` + pass/fail print pattern as
`i2c_init/target.rs`. The test harness (`system_image_test`) expects a
specific pass string on UART; match whatever `target_common::TargetInterface`
requires.

On failure, print the `I2cError` variant (reuse `i2c_error_str` from
`i2c_init/target.rs`) and the received bytes for diagnosis.

---

## 6. Build rule sketch

```python
system_image(
    name = "i2c_slave_rx",
    kernel = ":target",
    platform = "//target/ast10x0",
    system_config = ":system_config",
    tags = ["kernel"],
    userspace = False,
)

system_image_test(
    name = "i2c_slave_rx_test",
    image = ":i2c_slave_rx",
    tags = ["hardware"],              # EVB only
    target_compatible_with = select({
        "//target/ast10x0:qemu_enabled": ["@platforms//:incompatible"],
        "//conditions:default": [],
    }),
)
```

---

## 7. Preconditions / blockers

| # | Check | Status |
|---|-------|--------|
| 1 | `i2c_buses` in `target.rs` includes `(BUS_2, &SLAVE_CFG)` + `PINCTRL_I2C2` | ❓ wire into `Ast10x0BoardDescriptor` in target.rs |
| 2 | `poll_slave_data` / `read_slave_buffer` implemented on `Ast1060I2c` | ✅ `hal_slave_impl.rs` |
| 3 | External master available (AST2600 or second EVB) | ❓ arrange before running |
| 4 | I2C2 SDA/SCL/GND wired between EVBs with 4.7kΩ pull-ups | ❓ manual setup |
| 5 | `i2c_init` test passes on EVB (baseline) | ❓ run first |
| 6 | Agreed payload constant between slave binary and external master | ❓ coordinate |

Item 1 is the gating prerequisite: `board.init()` only calls `init_bus(2, ...)`
if Bus 2 appears in the `i2c_buses` slice passed to `Ast10x0BoardDescriptor`.

---

## 8. What is NOT in this plan

- IRQ / `Signals::USER` path — that is a separate slice (notification path);
  this test uses polling (`poll_slave_data`) only.
- Multi-byte or multi-message queue — single 4-byte write only.
- Slave TX / `ReadRequest` — explicitly deferred.
- IPC / server-runtime wiring — direct driver access only; no channel plumbing.
