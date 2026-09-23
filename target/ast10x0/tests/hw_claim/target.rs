// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Kernel for the conflicting-claim image. Never runs on hardware: the image
//! exists so the hardware claim check has something to reject.

#![no_std]
#![no_main]

use console_backend::console_backend_write_all;
use entry as _;
use target_common::{declare_target, TargetInterface};

pub struct Target {}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 HW Claim Conflict";

    fn main() -> ! {
        codegen::start();
        #[expect(clippy::empty_loop)]
        loop {}
    }

    fn shutdown(_code: u32) -> ! {
        let _ = console_backend_write_all(b"shutdown\n");
        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
