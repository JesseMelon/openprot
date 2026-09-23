// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Fails the build when two processes in one system image take the same
//! hardware.
//!
//! Reads the merged `.pw_kernel.annotations.hw_claim` section of an assembled
//! image. The section is a flat array of twelve-byte records appended by the
//! system assembler, one per claim, with the producing app's symbols gone.

use object::{Object, ObjectSection};

const SECTION: &str = ".pw_kernel.annotations.hw_claim";
const RECORD_LEN: usize = 12;
const KIND_MMIO: u32 = 0;

struct Mmio {
    start: u64,
    len: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let expect_conflict = args.iter().any(|a| a == "--expect-conflict");
    let elf_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .expect("usage: check <image.elf> [--expect-conflict]")
        .clone();

    let data = std::fs::read(&elf_path).expect("read image");
    let image = object::File::parse(&*data).expect("parse image");

    let claims = match image.section_by_name(SECTION) {
        Some(section) => section.data().expect("read claim section").to_vec(),
        None => Vec::new(),
    };

    if claims.len() % RECORD_LEN != 0 {
        eprintln!(
            "{SECTION} is {} bytes, not a whole number of {RECORD_LEN}-byte records",
            claims.len()
        );
        std::process::exit(1);
    }

    let mut mmio = Vec::new();
    for record in claims.chunks_exact(RECORD_LEN) {
        let field = |i: usize| u32::from_le_bytes(record[i * 4..i * 4 + 4].try_into().unwrap());
        match field(0) {
            KIND_MMIO => mmio.push(Mmio {
                start: u64::from(field(1)),
                len: u64::from(field(2)),
            }),
            kind => {
                eprintln!("unknown claim kind {kind}");
                std::process::exit(1);
            }
        }
    }

    mmio.sort_by_key(|m| m.start);
    let mut conflicts = 0;
    for pair in mmio.windows(2) {
        let (first, second) = (&pair[0], &pair[1]);
        if first.start + first.len > second.start {
            eprintln!(
                "two processes take overlapping MMIO: {:#010x}..{:#010x} and {:#010x}..{:#010x}",
                first.start,
                first.start + first.len,
                second.start,
                second.start + second.len,
            );
            conflicts += 1;
        }
    }

    if expect_conflict {
        if conflicts == 0 {
            eprintln!("{elf_path}: expected a hardware conflict, found none");
            std::process::exit(1);
        }
    } else if conflicts > 0 {
        eprintln!("{elf_path}: {conflicts} hardware conflict(s)");
        std::process::exit(1);
    }

    println!(
        "{elf_path}: {} claims checked, {conflicts} conflict(s)",
        mmio.len()
    );
}
