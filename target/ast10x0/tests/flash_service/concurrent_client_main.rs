// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Checks that the flash server keeps serving one chip select while an erase
//! on the other is in flight.
//!
//! A second thread erases a backed-up sector on one chip select in a loop and
//! records each erase's start and end time. Meanwhile the main thread reads a
//! fixed region of the other chip select as fast as it can, verifying every
//! read against a snapshot and recording when each read completed, and
//! programs pages of a freshly erased sector on that same chip select. The
//! test passes only if reads completed strictly inside every erase window and
//! at least one program did too: a server that blocks for the whole erase
//! cannot produce either. Both directions are run (erase CS1 while using CS0,
//! then erase CS0 while using CS1). Every sector the test erases is backed
//! up first and restored at the end.

#![no_main]
#![no_std]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use app_flash_client::handle;
use hal_flash::{Flash, FlashAddress};
use openprot_flash_client_ipc::{ChannelTransport, FlashIpcClient};
use pw_status::{Error, Result};
use userspace::time::{Clock, Instant, SystemClock};
use userspace::{entry, syscall};
use util_ipc::IpcHandle;
use util_types::PowerOf2Usize;

/// CS1 test sector: 1 MiB in, clear of anything the EVB boots from.
const CS1_TEST_OFFSET: usize = 0x0010_0000;
/// The largest sector the test backs up. The backend serves 4 KiB sectors.
const MAX_SECTOR: usize = 4096;
/// Bytes per read request on the reading side.
const READ_LEN: usize = 256;
/// Bytes per program request on the reading side.
const PROGRAM_LEN: usize = 256;
/// Erases the eraser thread issues per direction.
const ERASES: usize = 8;
/// Reads between two programs on the reading side.
const READS_PER_PROGRAM: usize = 16;
/// Most read completion times kept for the overlap count.
const MAX_READ_SAMPLES: usize = 1024;
/// Most program intervals kept.
const MAX_PROGRAMS: usize = MAX_SECTOR / PROGRAM_LEN;
/// Give up on the eraser after this long.
const ERASER_LIMIT_US: u32 = 30_000_000;
/// Stack of the eraser thread.
const ERASER_STACK_LEN: usize = 4096;

type Res<T> = core::result::Result<T, &'static str>;

fn addr(offset: usize) -> FlashAddress {
    FlashAddress::new(offset as u32)
}

fn pattern(i: usize, salt: u8) -> u8 {
    (i as u8).wrapping_mul(31).wrapping_add(salt)
}

/// Microseconds since boot, truncated. Intervals are computed with wrapping
/// subtraction so the truncation does not matter.
fn now_us() -> u32 {
    let ticks = SystemClock::now().ticks();
    (ticks / (SystemClock::TICKS_PER_SEC / 1_000_000)) as u32
}

/// `t` lies strictly inside `[start, end]`, all in wrapping microseconds.
fn inside(t: u32, start: u32, end: u32) -> bool {
    let len = end.wrapping_sub(start);
    let off = t.wrapping_sub(start);
    off > 0 && off < len
}

// ---------------------------------------------------------------------------
// Eraser thread. Its job and results go through statics, since the kernel
// starts it with a bare entry point.

const ZERO: AtomicU32 = AtomicU32::new(0);

static ERASE_HANDLE: AtomicU32 = ZERO;
static ERASE_ADDR: AtomicU32 = ZERO;
static ERASE_SIZE: AtomicU32 = ZERO;
/// 0 while running, 1 when every erase succeeded, 2 on failure.
static ERASE_DONE: AtomicU32 = ZERO;
/// Erases completed so far; `ERASE_WINDOWS[..count]` are valid.
static ERASE_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Start and end of each erase, in `now_us`.
const ZERO_PAIR: [AtomicU32; 2] = [ZERO, ZERO];
static ERASE_WINDOWS: [[AtomicU32; 2]; ERASES] = [ZERO_PAIR; ERASES];

#[repr(align(8))]
struct Stack(UnsafeCell<[u8; ERASER_STACK_LEN]>);
// SAFETY: only the eraser thread runs on this stack, and only while the main
// thread is waiting to join it or busy elsewhere; the main thread never reads
// or writes it.
unsafe impl Sync for Stack {}
static ERASER_STACK: Stack = Stack(UnsafeCell::new([0; ERASER_STACK_LEN]));

fn erase_loop() -> Res<()> {
    let transport = ChannelTransport::new(IpcHandle::new(ERASE_HANDLE.load(Ordering::SeqCst)));
    let mut flash = FlashIpcClient::new(transport).map_err(|_| "eraser connect")?;
    let target = FlashAddress::new(ERASE_ADDR.load(Ordering::SeqCst));
    let size = PowerOf2Usize::new(ERASE_SIZE.load(Ordering::SeqCst) as usize)
        .ok_or("eraser sector size")?;
    for (i, window) in ERASE_WINDOWS.iter().enumerate() {
        let start = now_us();
        flash.erase(target, size).map_err(|_| "eraser erase")?;
        let end = now_us();
        window[0].store(start, Ordering::SeqCst);
        window[1].store(end, Ordering::SeqCst);
        ERASE_COUNT.store(i + 1, Ordering::SeqCst);
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn eraser_entry(_arg: usize) -> ! {
    let code = match erase_loop() {
        Ok(()) => 0,
        Err(reason) => {
            pw_log::error!("eraser FAIL: {}", reason as &str);
            1
        }
    };
    ERASE_DONE.store(1 + code, Ordering::SeqCst);
    syscall::thread_exit(code);
}

fn start_eraser(erase_handle: u32, erase_at: usize, sector: PowerOf2Usize) -> Res<()> {
    ERASE_HANDLE.store(erase_handle, Ordering::SeqCst);
    ERASE_ADDR.store(erase_at as u32, Ordering::SeqCst);
    ERASE_SIZE.store(sector.get() as u32, Ordering::SeqCst);
    ERASE_COUNT.store(0, Ordering::SeqCst);
    ERASE_DONE.store(0, Ordering::SeqCst);
    let pc = eraser_entry as *const () as usize;
    let sp = ERASER_STACK.0.get() as usize + ERASER_STACK_LEN;
    syscall::thread_start(handle::FLASH_ERASER_THREAD, pc, sp).map_err(|_| "thread_start")
}

fn join_eraser() -> Res<()> {
    syscall::object_wait(
        handle::FLASH_ERASER_THREAD,
        syscall::Signals::JOINABLE,
        Instant::MAX,
    )
    .map_err(|_| "eraser wait")?;
    match syscall::task_join(handle::FLASH_ERASER_THREAD) {
        Ok(syscall::ExitStatus::Success(0)) => Ok(()),
        _ => Err("eraser thread failed"),
    }
}

// ---------------------------------------------------------------------------
// Reading side, on the main thread.

/// Read completion times and program intervals from one direction.
struct Samples {
    reads: usize,
    read_times: [u32; MAX_READ_SAMPLES],
    max_read_us: u32,
    mismatches: usize,
    programs: usize,
    program_windows: [[u32; 2]; MAX_PROGRAMS],
    program_failures: usize,
}

static SAMPLES: Shared<Samples> = Shared(UnsafeCell::new(Samples {
    reads: 0,
    read_times: [0; MAX_READ_SAMPLES],
    max_read_us: 0,
    mismatches: 0,
    programs: 0,
    program_windows: [[0; 2]; MAX_PROGRAMS],
    program_failures: 0,
}));

struct Shared<T>(UnsafeCell<T>);
// SAFETY: `SAMPLES` is only touched by the main thread; it is a static to
// keep the sample arrays off the thread stack.
unsafe impl<T> Sync for Shared<T> {}

fn samples() -> &'static mut Samples {
    // SAFETY: see `Shared`. One mutable reference at a time, on one thread.
    unsafe { &mut *SAMPLES.0.get() }
}

/// What the main thread does with the chip select the eraser is not using.
struct ReadSide {
    /// Region read and verified in a loop. Never written by the test.
    read_at: usize,
    /// Sector the main thread erases before the eraser starts, then programs
    /// page by page while the eraser runs. Backed up by the caller.
    program_sector: usize,
    sector: PowerOf2Usize,
}

/// Runs the reading side until the eraser is done. Returns the samples.
fn read_side(flash: &mut impl Flash, side: ReadSide) -> Res<&'static mut Samples> {
    let s = samples();
    *s = Samples {
        reads: 0,
        read_times: [0; MAX_READ_SAMPLES],
        max_read_us: 0,
        mismatches: 0,
        programs: 0,
        program_windows: [[0; 2]; MAX_PROGRAMS],
        program_failures: 0,
    };
    let mut reference = [0u8; READ_LEN];
    flash
        .read(addr(side.read_at), &mut reference)
        .map_err(|_| "snapshot read")?;

    let pages = side.sector.get() / PROGRAM_LEN;
    let started = now_us();
    let mut buf = [0u8; READ_LEN];
    loop {
        if ERASE_DONE.load(Ordering::SeqCst) != 0 {
            break;
        }
        if now_us().wrapping_sub(started) > ERASER_LIMIT_US {
            return Err("eraser did not finish");
        }
        let t0 = now_us();
        flash
            .read(addr(side.read_at), &mut buf)
            .map_err(|_| "read during erase")?;
        let t1 = now_us();
        if buf != reference {
            s.mismatches += 1;
        }
        if let Some(slot) = s.read_times.get_mut(s.reads) {
            *slot = t1;
        }
        s.reads += 1;
        s.max_read_us = s.max_read_us.max(t1.wrapping_sub(t0));

        if s.reads % READS_PER_PROGRAM == 0 && s.programs < pages.min(MAX_PROGRAMS) {
            let page = s.programs;
            let at = side.program_sector + page * PROGRAM_LEN;
            let mut data = [0u8; PROGRAM_LEN];
            for (i, b) in data.iter_mut().enumerate() {
                *b = pattern(i, page as u8);
            }
            let p0 = now_us();
            let programmed = flash.program(addr(at), &data);
            let p1 = now_us();
            let mut back = [0u8; PROGRAM_LEN];
            let verified = programmed.is_ok()
                && flash.read(addr(at), &mut back).is_ok()
                && back == data;
            if !verified {
                s.program_failures += 1;
            }
            if let Some(w) = s.program_windows.get_mut(page) {
                *w = [p0, p1];
            }
            s.programs += 1;
        }
    }
    Ok(s)
}

/// Evaluates one direction's samples against the eraser's windows.
fn evaluate(name: &'static str, s: &Samples) -> Res<()> {
    let erases = ERASE_COUNT.load(Ordering::SeqCst);
    let mut min_erase_us = u32::MAX;
    let mut max_erase_us = 0u32;
    let mut windows_with_reads = 0usize;
    let mut reads_inside = 0usize;
    let kept = s.reads.min(MAX_READ_SAMPLES);
    for window in ERASE_WINDOWS.iter().take(erases) {
        let (start, end) = (
            window[0].load(Ordering::SeqCst),
            window[1].load(Ordering::SeqCst),
        );
        let len = end.wrapping_sub(start);
        min_erase_us = min_erase_us.min(len);
        max_erase_us = max_erase_us.max(len);
        let inside_here = s.read_times[..kept]
            .iter()
            .filter(|&&t| inside(t, start, end))
            .count();
        if inside_here > 0 {
            windows_with_reads += 1;
        }
        reads_inside += inside_here;
    }
    let programs_inside = s.program_windows[..s.programs.min(MAX_PROGRAMS)]
        .iter()
        .filter(|[p0, p1]| {
            ERASE_WINDOWS.iter().take(erases).any(|w| {
                let (start, end) = (w[0].load(Ordering::SeqCst), w[1].load(Ordering::SeqCst));
                inside(*p0, start, end) && inside(*p1, start, end)
            })
        })
        .count();

    // Several short lines: the tokenized logger drops a line with many arguments.
    pw_log::info!("{}:", name as &str);
    pw_log::info!(
        "  erases {} taking {}..{} us",
        erases as u32,
        min_erase_us as u32,
        max_erase_us as u32
    );
    pw_log::info!(
        "  reads {} (max latency {} us), {} inside erases, {} of {} erases saw reads",
        s.reads as u32,
        s.max_read_us as u32,
        reads_inside as u32,
        windows_with_reads as u32,
        erases as u32
    );
    pw_log::info!(
        "  programs {} ({} inside erases), mismatches {}, program failures {}",
        s.programs as u32,
        programs_inside as u32,
        s.mismatches as u32,
        s.program_failures as u32
    );

    if erases != ERASES {
        return Err("not every erase completed");
    }
    if s.mismatches != 0 {
        return Err("a read during an erase returned wrong data");
    }
    if s.program_failures != 0 {
        return Err("a program during an erase failed or read back wrong");
    }
    if windows_with_reads != erases {
        return Err("an erase window had no read inside it: the server blocked");
    }
    if programs_inside == 0 {
        return Err("no program completed inside an erase window");
    }
    Ok(())
}

/// One direction: erase `erase_at` on `erase_handle` while `flash` serves
/// `side`. `flash` must be the other chip select's client.
fn run_direction(
    name: &'static str,
    flash: &mut impl Flash,
    side: ReadSide,
    erase_handle: u32,
    erase_at: usize,
    sector: PowerOf2Usize,
) -> Res<()> {
    // Erased up front so the programs during the window have a clean sector.
    flash
        .erase(addr(side.program_sector), side.sector)
        .map_err(|_| "pre-erase program sector")?;
    start_eraser(erase_handle, erase_at, sector)?;
    let outcome = read_side(flash, side);
    let joined = join_eraser();
    let s = outcome?;
    joined?;
    evaluate(name, s)
}

// ---------------------------------------------------------------------------
// Geometry, backup and restore.

#[derive(Clone, Copy)]
struct Geometry {
    sector: PowerOf2Usize,
    total: usize,
}

fn geometry(flash: &mut impl Flash) -> Res<Geometry> {
    let (total, sector, erase_sizes) = flash.geometry().map_err(|_| "geometry")?;
    let len = sector.get();
    if len > MAX_SECTOR || len < 2 * PROGRAM_LEN || erase_sizes != 1 << len.trailing_zeros() {
        return Err("unsupported sector size");
    }
    if total.get() % len != 0 || total.get() < 4 * len {
        return Err("flash too small");
    }
    Ok(Geometry {
        sector,
        total: total.get(),
    })
}

fn backup(flash: &mut impl Flash, at: usize, sector: PowerOf2Usize, out: &mut [u8]) -> Res<()> {
    let out = out.get_mut(..sector.get()).ok_or("backup buffer")?;
    flash.read(addr(at), out).map_err(|_| "backup read")
}

fn restore(flash: &mut impl Flash, at: usize, sector: PowerOf2Usize, saved: &[u8]) -> Res<()> {
    let saved = saved.get(..sector.get()).ok_or("restore buffer")?;
    flash
        .erase(addr(at), sector)
        .map_err(|_| "restore erase")?;
    flash
        .program(addr(at), saved)
        .map_err(|_| "restore program")?;
    let mut buf = [0u8; PROGRAM_LEN];
    for (i, expected) in saved.chunks(PROGRAM_LEN).enumerate() {
        let buf = buf.get_mut(..expected.len()).ok_or("restore buffer")?;
        flash
            .read(addr(at + i * PROGRAM_LEN), buf)
            .map_err(|_| "restore read")?;
        if buf != expected {
            return Err("restore verify");
        }
    }
    Ok(())
}

static BACKUP0: Shared<[u8; MAX_SECTOR]> = Shared(UnsafeCell::new([0; MAX_SECTOR]));
static BACKUP1: Shared<[u8; MAX_SECTOR]> = Shared(UnsafeCell::new([0; MAX_SECTOR]));

fn run_all() -> Res<()> {
    let mut cs0 = FlashIpcClient::new(ChannelTransport::new(IpcHandle::new(handle::FLASH_CS0)))
        .map_err(|_| "connect cs0")?;
    let mut cs1 = FlashIpcClient::new(ChannelTransport::new(IpcHandle::new(handle::FLASH_CS1)))
        .map_err(|_| "connect cs1")?;
    let g0 = geometry(&mut cs0)?;
    let g1 = geometry(&mut cs1)?;

    // Sectors the test erases: the last sector of the CS0 boot flash and the
    // sector 1 MiB into CS1. Regions it only reads: the sector before each.
    let erase0 = g0.total - g0.sector.get();
    let read0 = erase0 - g0.sector.get();
    let erase1 = CS1_TEST_OFFSET;
    let read1 = CS1_TEST_OFFSET + g1.sector.get();
    if erase1 % g1.sector.get() != 0 || g1.total < read1 + g1.sector.get() {
        return Err("cs1 layout does not fit");
    }

    // SAFETY: see `Shared`; main thread only.
    let (saved0, saved1) = unsafe { (&mut *BACKUP0.0.get(), &mut *BACKUP1.0.get()) };
    backup(&mut cs0, erase0, g0.sector, saved0)?;
    backup(&mut cs1, erase1, g1.sector, saved1)?;

    let result = run_direction(
        "erase cs1 while using cs0",
        &mut cs0,
        ReadSide {
            read_at: read0,
            program_sector: erase0,
            sector: g0.sector,
        },
        handle::FLASH_CS1,
        erase1,
        g1.sector,
    )
    .and_then(|()| {
        run_direction(
            "erase cs0 while using cs1",
            &mut cs1,
            ReadSide {
                read_at: read1,
                program_sector: erase1,
                sector: g1.sector,
            },
            handle::FLASH_CS0,
            erase0,
            g0.sector,
        )
    });

    // Restore both sectors even if a check failed.
    let restored0 = restore(&mut cs0, erase0, g0.sector, saved0);
    let restored1 = restore(&mut cs1, erase1, g1.sector, saved1);
    result.and(restored0).and(restored1)
}

#[entry]
fn entry() -> Result<()> {
    let result = run_all().map_err(|reason| {
        pw_log::error!("flash concurrency client FAIL: {}", reason as &str);
        Error::Internal
    });
    if result.is_ok() {
        pw_log::info!("flash concurrency client PASS");
    }
    let _ = syscall::debug_shutdown(result);
    result
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = syscall::debug_shutdown(Err(Error::Internal));
    #[expect(clippy::empty_loop)]
    loop {}
}
