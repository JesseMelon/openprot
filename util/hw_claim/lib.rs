// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Records of the hardware a process actually takes.
//!
//! Every app links on its own and the assembler renames each app's symbols
//! apart, so nothing compares one app's hardware use to another's.
//! `.pw_kernel.annotations.hw_claim` is one of the few sections the assembler
//! appends instead of renaming, so records from every app land in one section of
//! the assembled image and a host check can see the same hardware taken twice.

#![no_std]

pub use util_region::Mmap;

/// An MMIO range: `a` is the start address, `b` the length in bytes. Two
/// records conflict when their ranges overlap.
pub const KIND_MMIO: u32 = 0;

/// One pin: `a` is the controller's base address, `b` the pin index. Two
/// records conflict when both fields match.
pub const KIND_PIN: u32 = 1;

/// Room for a label like `pldm_fd: fmc_cs1_window`, NUL-padded.
pub const NAME_LEN: usize = 52;

/// One piece of hardware a process has taken.
///
/// The assembler concatenates raw bytes and drops the symbols, so a record has
/// to name itself and every record has to be the same size. Four-byte fields and
/// a name length that is a multiple of four mean the linker never pads between
/// records, so the host reader can walk the merged section by stride.
#[repr(C)]
pub struct Claim {
    pub kind: u32,
    pub a: u32,
    pub b: u32,
    pub name: [u8; NAME_LEN],
}

/// Joins the claiming crate and the thing claimed into a fixed-width label.
///
/// Truncates rather than failing: a name is for the error message, and a build
/// that stops because a label was long would be worse than a clipped one.
pub const fn label(krate: &str, item: &str) -> [u8; NAME_LEN] {
    let mut out = [0u8; NAME_LEN];
    let mut at = 0;

    let krate = krate.as_bytes();
    let mut i = 0;
    while i < krate.len() && at < NAME_LEN {
        out[at] = krate[i];
        at += 1;
        i += 1;
    }

    if at < NAME_LEN {
        out[at] = b':';
        at += 1;
    }
    if at < NAME_LEN {
        out[at] = b' ';
        at += 1;
    }

    let item = item.as_bytes();
    let mut i = 0;
    while i < item.len() && at < NAME_LEN {
        out[at] = item[i];
        at += 1;
        i += 1;
    }

    out
}

/// Records that this process takes the MMIO range of each named mapping.
///
/// Place it beside the code that hands the matching `Region` to a driver. The
/// manifest says the process *may* have the range; this says it does.
#[macro_export]
macro_rules! claim_mmio {
    ($($mmap:ty),+ $(,)?) => {$(
        const _: () = {
            #[used]
            #[unsafe(link_section = ".pw_kernel.annotations.hw_claim")]
            static CLAIM: $crate::Claim = $crate::Claim {
                kind: $crate::KIND_MMIO,
                a: <$mmap as $crate::Mmap>::START as u32,
                b: <$mmap as $crate::Mmap>::LEN as u32,
                name: $crate::label(module_path!(), stringify!($mmap)),
            };
        };
    )+};
}

/// Records that this process drives one pin of a controller.
///
/// Several processes may legitimately be granted the same controller's
/// registers, so the MMIO range cannot say who drives which pin. This can.
#[macro_export]
macro_rules! claim_pin {
    ($name:literal, $controller:expr, $index:expr) => {
        const _: () = {
            #[used]
            #[unsafe(link_section = ".pw_kernel.annotations.hw_claim")]
            static CLAIM: $crate::Claim = $crate::Claim {
                kind: $crate::KIND_PIN,
                a: $controller as u32,
                b: $index as u32,
                name: $crate::label(module_path!(), $name),
            };
        };
    };
}
