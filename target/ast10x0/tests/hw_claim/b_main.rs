// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Takes the FMC registers. So does `a_main.rs`.

#![no_main]
#![no_std]

use app_claim_b_regions::FmcRegs;
use userspace::entry;
use util_hw_claim::claim_mmio;

claim_mmio!(FmcRegs);

#[entry]
fn entry() {
    #[expect(clippy::empty_loop)]
    loop {}
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
