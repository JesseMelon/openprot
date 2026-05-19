// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! I2C server app: board init + open Bus 2 + run the server-runtime loop.
//!
//! The server owns all hardware access. It registers the I2C channel and the
//! I2C2 IRQ with a WaitGroup and dispatches forever via
//! `i2c_server_runtime::run()`.

#![no_main]
#![no_std]

use app_i2c_server::{handle, signals};
use ast10x0_peripherals::i2c::{ClockConfig, I2cConfig, I2cSpeed, I2cXferMode};
use i2c_server_runtime::{Bus, run};
use userspace::entry;

const SLAVE_CFG: I2cConfig = I2cConfig {
    speed: I2cSpeed::Standard,
    xfer_mode: I2cXferMode::DmaMode,
    multi_master: false,
    smbus_timeout: false,
    smbus_alert: false,
    clock_config: ClockConfig::ast1060_default(),
};

// Non-cached SRAM buffer for DMA — must be visible to both the DMA engine and
// the CPU without cache aliasing.  The linker places `.ram_nc` sections in the
// non-cached SRAM window of the AST1060.
#[unsafe(link_section = ".ram_nc")]
static mut DMA_BUF: [u8; 256] = [0u8; 256];

#[entry]
fn entry() {
    // Phase A: per-controller init (timing, master-enable, interrupts).
    // SCU/global/pinctrl were already done by the kernel in target.rs.
    // SAFETY: server process exclusively owns Bus 2; kernel init is complete.
    if unsafe { i2c_backend::init_bus(2, &SLAVE_CFG) }.is_err() {
        pw_log::error!("init_bus(2) failed");
        loop {}
    }

    // Phase B: wrap the initialised controller in DMA mode.
    // SAFETY: init_bus(2) done above; DMA_BUF is in non-cached SRAM
    // and uniquely owned by this bus for the driver's lifetime.
    let dma_buf: &'static mut [u8] =
        unsafe { &mut *core::ptr::addr_of_mut!(DMA_BUF) };
    let driver = match unsafe { i2c_backend::open_bus_dma(2, &SLAVE_CFG, dma_buf) } {
        Ok(d) => d,
        Err(_) => {
            pw_log::error!("open_bus_dma(2) failed");
            loop {}
        }
    };

    pw_log::info!("I2C server ready on Bus 2");

    let mut buses = [Bus::new(handle::I2C, driver)];
    run(handle::WG, handle::I2C2_IRQ, signals::I2C2, &mut buses);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
