// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! MCTP-over-I2C transport test — receiver side (device B)
//!
//! Runs on the AST1060 Test Harness board with J15 pins 1 and 2 connected.
//! This binary is the RECEIVER (I2C slave). Load it on device B before loading
//! the sender image on device A.
//!
//! Listens at address 0x42 for an MCTP-over-I2C frame, decodes it, and
//! verifies the payload matches what the sender transmits.

#![no_std]
#![no_main]

use ast10x0_board::{Ast10x0Board, Ast10x0BoardDescriptor};
use ast10x0_peripherals::i2c::{
    Ast1060I2c, Ast1060I2cRegisters, ClockConfig, I2cConfig, I2cSpeed, I2cXferMode, SlaveConfig,
    SlaveEvent,
};
use ast10x0_peripherals::scu::pinctrl;
use codegen as _;
use console_backend::console_backend_write_all;
use entry as _;
use openprot_mctp_transport_i2c::MctpI2cReceiver;
use target_common::{TargetInterface, declare_target};

pub struct Target {}

const SLAVE_ADDR: u8 = 0x42;
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

fn wait_data_received<Y: FnMut(u32)>(
    slave: &mut Ast1060I2c<'_, Y>,
    max_polls: u32,
) -> Option<SlaveEvent> {
    for _ in 0..max_polls {
        match slave.handle_slave_interrupt() {
            // WriteRequest and Stop are normal sequencing events in a write
            // transaction; keep polling past them.
            Some(SlaveEvent::WriteRequest) | Some(SlaveEvent::Stop) | None => {}
            Some(ev) => return Some(ev),
        }
        core::hint::spin_loop();
    }
    None
}

fn run_receiver() -> Result<(), &'static str> {
    pw_log::info!("=== MCTP-over-I2C transport test: RECEIVER (device B) ===");
    pw_log::info!(
        "Listening at addr 0x{:02x}. Start sender (device A) now.",
        SLAVE_ADDR as u32
    );

    let board = Ast10x0Board::new(Ast10x0BoardDescriptor {
        pinctrl_groups: &[pinctrl::PINCTRL_I2C2],
        i2c_buses: &[],
    });
    // SAFETY: single call at boot with exclusive access to SCU/I2C global regs.
    unsafe { board.init() }.map_err(|_| "board init failed")?;

    // SAFETY: I2C2 registers accessed only through `slave` for this test.
    let mut slave = unsafe {
        let mmio =
            Ast1060I2cRegisters::new(ast1060_pac::I2c2::ptr(), ast1060_pac::I2cbuff2::ptr());
        Ast1060I2c::new(mmio, &i2c2_config(), |_| core::hint::spin_loop())
    }
    .map_err(|_| "I2C2 init failed")?;

    let slave_cfg = SlaveConfig::new(SLAVE_ADDR).map_err(|_| "SlaveConfig::new failed")?;
    slave
        .configure_slave(&slave_cfg)
        .map_err(|_| "configure_slave failed")?;

    let receiver = MctpI2cReceiver::new(SLAVE_ADDR);

    match wait_data_received(&mut slave, 100_000_000) {
        Some(SlaveEvent::DataReceived { len }) => {
            let mut raw = [0u8; 128];
            let n = len.min(raw.len());
            slave
                .slave_read(&mut raw[..n])
                .map_err(|_| "slave_read failed")?;

            // BufferMode slave delivers [cmd][bc][src][mctp_hdr...][pec] without the
            // leading dest-addr byte. MctpI2cReceiver::decode expects the full SMBus
            // frame [dest_addr_W][cmd][bc][src][...][pec], so we prepend it here.
            let mut frame = [0u8; 129];
            frame[0] = SLAVE_ADDR << 1;
            frame[1..n + 1].copy_from_slice(&raw[..n]);

            let (pkt, _) = receiver
                .decode(&frame[..n + 1])
                .map_err(|_| "MCTP decode failed")?;

            // MCTP packet layout: [hdr_ver][dest_eid][src_eid][flags][msg_type][payload...]
            if pkt.len() < 5 {
                return Err("MCTP packet too short");
            }
            if pkt[4] & 0x7F != ECHO_MSG_TYPE {
                pw_log::error!(
                    "msg type: got 0x{:02x}, expected 0x{:02x}",
                    (pkt[4] & 0x7F) as u32,
                    ECHO_MSG_TYPE as u32
                );
                return Err("message type mismatch");
            }
            if &pkt[5..] != ECHO_PAYLOAD {
                return Err("payload mismatch");
            }
            pw_log::info!("MCTP frame decoded and payload verified");
        }
        Some(_) => return Err("unexpected slave event"),
        None => return Err("timed out waiting for MCTP frame"),
    }

    Ok(())
}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 MCTP I2C Transport Receiver";

    fn main() -> ! {
        let sentinel: &[u8] = match run_receiver() {
            Ok(()) => b"TEST_RESULT:PASS\n",
            Err(e) => {
                pw_log::error!("Receiver test failed: {}", e as &str);
                b"TEST_RESULT:FAIL\n"
            }
        };
        let _ = console_backend_write_all(sentinel);
        #[expect(clippy::empty_loop)]
        loop {}
    }
}

declare_target!(Target);
