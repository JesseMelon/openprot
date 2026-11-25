# flash_service

Tests the flash service over kernel IPC on the AST10x0. A flash server process
owns the FMC and serves its CS0 flash on the `flash_cs0` channel and its CS1
flash on the `flash_cs1` channel, one `Server` per channel, through
`services/flash/server-runtime`. Both drivers share one `FmcController`. A
flash client process reaches each channel through `services/flash/client-ipc`. This is the AST10x0 counterpart of
`target/veer/tests/flash_service`. The `smc/*` tests drive the controller
directly in one task and do not cover the IPC path.

The client takes the sector size and flash size the server reports, which the
driver discovers over SFDP, and checks that they are consistent instead of
comparing them with fixed values. The service does not report the program
page, so the test starts its program just before the middle of the sector,
which is a program page boundary for any page smaller than the sector.

Then, on each channel, it erases, programs (across a program page boundary) and
reads back one sector. It also checks that a wrong-size erase, an unaligned
erase and a read past the end are rejected. Last, it checks that each channel
reaches its own flash: it programs a pattern through one channel and checks the
other channel's flash is unchanged, in both directions. It backs each sector up
first and restores it at the end, even if a check failed, so it leaves the flash
as it found it on the EVB. CS0 is the boot flash on the EVB, so its sector is
the last one of the flash, which the boot image does not reach. On CS1 it is
1 MiB in.

The kernel target applies the FMC pinmux before it starts any process, so no
task maps the SCU.

## Running

QEMU seeds erased 64 MiB images on both chip selects and models a
`w25q512jv`:

```
bazel test --config=virt_ast10x0 //target/ast10x0/tests/flash_service:flash_qemu_test
```

EVB hardware, with the boot flash on CS0 and the W25Q512 on CS1. This runs on the AST1060 EVB through
the Pi fixture, so `AST1060_EVB_PI_HOST` must name the Pi host. Without it the
test fails:

```
AST1060_EVB_PI_HOST=<pi_host> bazel test --config=k_ast1060_evb //target/ast10x0/tests/flash_service:flash_evb_test
```

See [`../README.md`](../README.md) for the fixture setup.

## Concurrency variant

`flash_concurrent` is the same image with a client process of two threads, and
checks that an erase on one chip select does not block the other. One thread
erases a backed-up sector on one chip select eight times and records each
erase's start and end. Meanwhile the main thread reads a fixed region of the
other chip select in a loop, verifying every read against a snapshot and
recording when it completed, and programs pages of a freshly erased sector on
that same chip select. The test passes only if reads completed inside every
erase window and at least one program did too, which a server that blocks for
the whole erase cannot produce. It runs both directions and restores every
sector it erased. It is hardware only: QEMU's flash model erases instantly, so
there is no window to land in.

```
AST1060_EVB_PI_HOST=<pi_host> bazel test --config=k_ast1060_evb //target/ast10x0/tests/flash_service:flash_concurrent_evb_test
```
