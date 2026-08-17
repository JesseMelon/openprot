// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! AST1060 BMC bring-up image: pulse GPIOM5 (`RoT_BMC_RESET_L`) to boot the BMC.

#![no_std]
#![no_main]

use ast10x0_peripherals::gpio::{gpiom, GpioExt};
use ast10x0_peripherals::scu::{pinctrl, ScuRegisters};
use console_backend::console_backend_write_all;
use embedded_hal::digital::OutputPin;
use target_common::{declare_target, TargetInterface};
use {console_backend as _, entry as _};

pub struct Target {}

/// Busy-wait iterations for the reset-assert hold; tune once observed on hardware.
const RESET_HOLD_ITERS: u32 = 2_000_000;

fn bring_up_bmc() {
    // SAFETY: this kernel-only image is the sole owner of the SCU and GPIO peripherals.
    let mut bmc_rst = unsafe {
        let scu = ScuRegisters::new_global_unlocked();
        scu.apply_pinctrl_group(pinctrl::PINCTRL_GPIOM5);
        gpiom::GPIOM::new_global().split().pm5.into_push_pull_output()
    };

    let _ = bmc_rst.set_low();
    pw_log::info!("BMC reset asserted (GPIOM5 low)");
    for _ in 0..RESET_HOLD_ITERS {
        core::hint::spin_loop();
    }

    let _ = bmc_rst.set_high();
    pw_log::info!("BMC reset released (GPIOM5 high)");
}

impl TargetInterface for Target {
    const NAME: &'static str = "OpenPRoT AST1060 BMC bring-up";

    fn main() -> ! {
        pw_log::info!("=== OpenPRoT AST1060 BMC bring-up ===");
        bring_up_bmc();
        let _ = console_backend_write_all(b"openprot bmc-bringup: BMC released from reset\n");

        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
