// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! MCTP-over-I2C transport test — sender side (device A)
//!
//! Runs on the AST1060 Test Harness board with J15 pins 1 and 2 connected,
//! which links I2C2 between device A and device B.
//!
//! This binary is the SENDER (I2C master). Load the companion receiver binary
//! on device B first, then load this image on device A.
//!
//! Sends one MCTP message over I2C and reports PASS if the write succeeds.

#![no_std]
#![no_main]

use ast10x0_board::{Ast10x0Board, Ast10x0BoardDescriptor};
use ast10x0_peripherals::i2c::{
    Ast1060I2c, Ast1060I2cRegisters, ClockConfig, I2cConfig, I2cSpeed, I2cXferMode,
};
use ast10x0_peripherals::scu::pinctrl;
use codegen as _;
use console_backend::console_backend_write_all;
use entry as _;
use openprot_mctp_transport_i2c::I2cSender;
use target_common::{TargetInterface, declare_target};

pub struct Target {}

const OWN_I2C_ADDR: u8 = 0x10;
const SLAVE_I2C_ADDR: u8 = 0x42;
const OWN_EID: u8 = 8;
const REMOTE_EID: u8 = 9;
const ECHO_MSG_TYPE: u8 = 1;
const ECHO_PAYLOAD: &[u8] = b"mctp_i2c_hw";

fn i2c2_config() -> I2cConfig {
    I2cConfig {
        xfer_mode: I2cXferMode::BufferMode,
        speed: I2cSpeed::Fast,
        multi_master: false,
        smbus_timeout: true,
        smbus_alert: false,
        clock_config: ClockConfig::ast1060_default(),
    }
}

fn run_sender() -> Result<(), &'static str> {
    pw_log::info!("=== MCTP-over-I2C transport test: SENDER (device A) ===");
    pw_log::info!("J15 must be connected. Load receiver image on device B first.");

    let board = Ast10x0Board::new(Ast10x0BoardDescriptor {
        pinctrl_groups: &[pinctrl::PINCTRL_I2C2],
        i2c_buses: &[],
    });
    // SAFETY: single call at boot with exclusive access to SCU/I2C global regs.
    unsafe { board.init() }.map_err(|_| "board init failed")?;

    // SAFETY: I2C2 registers accessed only through `i2c` for this test.
    let i2c = unsafe {
        let mmio =
            Ast1060I2cRegisters::new(ast1060_pac::I2c2::ptr(), ast1060_pac::I2cbuff2::ptr());
        Ast1060I2c::new(mmio, &i2c2_config(), |_| core::hint::spin_loop())
    }
    .map_err(|_| "I2C2 init failed")?;

    let sender = I2cSender::new(i2c, OWN_I2C_ADDR, SLAVE_I2C_ADDR);
    let mut server: openprot_mctp_server::Server<_, 4> =
        openprot_mctp_server::Server::new(mctp::Eid(OWN_EID), 0, sender);

    pw_log::info!("Sending MCTP message to EID {} via I2C 0x{:02x}", REMOTE_EID as u32, SLAVE_I2C_ADDR as u32);
    let req = server.req(REMOTE_EID).map_err(|_| "server.req failed")?;
    server
        .send(Some(req), ECHO_MSG_TYPE, None, None, false, ECHO_PAYLOAD)
        .map_err(|_| "server.send failed")?;

    pw_log::info!("MCTP message sent successfully");
    Ok(())
}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 MCTP I2C Transport Sender";

    fn main() -> ! {
        let sentinel: &[u8] = match run_sender() {
            Ok(()) => b"TEST_RESULT:PASS\n",
            Err(e) => {
                pw_log::error!("Sender test failed: {}", e as &str);
                b"TEST_RESULT:FAIL\n"
            }
        };
        let _ = console_backend_write_all(sentinel);
        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
