// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! MCTP response-channel test — sender side (device A)
//!
//! Runs on the AST1060 Test Harness board with J15 pins 1 and 2 connected,
//! which links I2C2 between device A and device B.
//!
//! This binary is the SENDER (I2C master). Load the companion receiver binary
//! on device B first, then load this image on device A.
//!
//! Sends one MCTP request over I2C, then switches to slave mode so that
//! device B's response frame can be ACKed on the bus. Reports PASS once the
//! response frame arrives (payload validation is deferred to the req_recv test).
//!
//! API surface exercised:
//!   Stack::new, Stack::req → MctpReqChannel
//!   MctpReqChannel::send   (request path: handle=Some, Tag::Owned)
//!   DirectClient / MctpClient::send → Server::send → I2cSender

#![no_std]
#![no_main]

use ast10x0_board::{Ast10x0Board, Ast10x0BoardDescriptor};
use ast10x0_peripherals::i2c::{
    Ast1060I2c, Ast1060I2cRegisters, ClockConfig, I2cConfig, I2cSpeed, I2cXferMode,
    SlaveConfig, SlaveEvent,
};
use ast10x0_peripherals::scu::pinctrl;
use codegen as _;
use console_backend::console_backend_write_all;
use entry as _;
use openprot_mctp_api::{Handle, MctpClient, MctpError, MctpReqChannel, RecvMetadata};
use openprot_mctp_api::stack::Stack;
use openprot_mctp_transport_i2c::I2cSender;
use target_common::{TargetInterface, declare_target};

pub struct Target {}

const OWN_I2C_ADDR: u8   = 0x10;
const SLAVE_I2C_ADDR: u8  = 0x42;
const OWN_EID: u8         = 8;
const REMOTE_EID: u8      = 9;
const MSG_TYPE: u8         = 1;
const REQUEST_PAYLOAD: &[u8] = b"mctp_req_hw";

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

// ---------------------------------------------------------------------------
// DirectClient — same shim as stack_api, wraps Server behind MctpClient trait
// ---------------------------------------------------------------------------

struct DirectClient<S: mctp_lib::Sender, const N: usize> {
    server: core::cell::UnsafeCell<openprot_mctp_server::Server<S, N>>,
}

impl<S: mctp_lib::Sender, const N: usize> DirectClient<S, N> {
    fn new(server: openprot_mctp_server::Server<S, N>) -> Self {
        DirectClient { server: core::cell::UnsafeCell::new(server) }
    }

    fn server_mut(&self) -> &mut openprot_mctp_server::Server<S, N> {
        // SAFETY: single-threaded bare-metal; no concurrent access.
        unsafe { &mut *self.server.get() }
    }
}

impl<S: mctp_lib::Sender, const N: usize> MctpClient for DirectClient<S, N> {
    fn req(&self, eid: u8) -> Result<Handle, MctpError> {
        self.server_mut().req(eid)
    }
    fn listener(&self, msg_type: u8) -> Result<Handle, MctpError> {
        self.server_mut().listener(msg_type)
    }
    fn get_eid(&self) -> u8 { self.server_mut().get_eid() }
    fn set_eid(&self, eid: u8) -> Result<(), MctpError> {
        self.server_mut().set_eid(eid)
    }
    fn recv(&self, handle: Handle, _timeout_millis: u32, buf: &mut [u8])
        -> Result<RecvMetadata, MctpError>
    {
        loop {
            if let Some(meta) = self.server_mut().try_recv(handle, buf) {
                return Ok(meta);
            }
            core::hint::spin_loop();
        }
    }
    fn send(
        &self,
        handle: Option<Handle>,
        msg_type: u8,
        eid: Option<u8>,
        tag: Option<u8>,
        integrity_check: bool,
        buf: &[u8],
    ) -> Result<u8, MctpError> {
        self.server_mut().send(handle, msg_type, eid, tag, integrity_check, buf)
    }
    fn drop_handle(&self, handle: Handle) {
        let _ = self.server_mut().unbind(handle);
    }
}

// ---------------------------------------------------------------------------

fn wait_data_received<Y: FnMut(u32)>(
    slave: &mut Ast1060I2c<'_, Y>,
    max_polls: u32,
) -> Option<SlaveEvent> {
    for _ in 0..max_polls {
        match slave.handle_slave_interrupt() {
            Some(SlaveEvent::WriteRequest) | Some(SlaveEvent::Stop) | None => {}
            Some(ev) => return Some(ev),
        }
        core::hint::spin_loop();
    }
    None
}

fn run_sender() -> Result<(), &'static str> {
    pw_log::info!("=== MCTP resp-channel test: SENDER (device A) ===");
    pw_log::info!("J15 must be connected. Load receiver image on device B first.");

    let board = Ast10x0Board::new(Ast10x0BoardDescriptor {
        pinctrl_groups: &[pinctrl::PINCTRL_I2C2],
        i2c_buses: &[],
    });
    // SAFETY: single call at boot with exclusive access to SCU/I2C global regs.
    unsafe { board.init() }.map_err(|_| "board init failed")?;

    // Phase 1: master — send the request.
    // SAFETY: I2C2 registers accessed only through `i2c` until it is dropped below.
    let i2c = unsafe {
        let mmio =
            Ast1060I2cRegisters::new(ast1060_pac::I2c2::ptr(), ast1060_pac::I2cbuff2::ptr());
        Ast1060I2c::new(mmio, &i2c2_config(), |_| core::hint::spin_loop())
    }
    .map_err(|_| "I2C2 init failed")?;

    let i2c_sender = I2cSender::new(i2c, OWN_I2C_ADDR, SLAVE_I2C_ADDR);
    let server: openprot_mctp_server::Server<_, 4> =
        openprot_mctp_server::Server::new(mctp::Eid(OWN_EID), 0, i2c_sender);
    let stack = Stack::new(DirectClient::new(server));

    pw_log::info!(
        "Sending MCTP request to EID {} via I2C 0x{:02x}",
        REMOTE_EID as u32,
        SLAVE_I2C_ADDR as u32,
    );
    let mut req = stack.req(REMOTE_EID, 0).map_err(|_| "stack.req failed")?;
    req.send(MSG_TYPE, REQUEST_PAYLOAD).map_err(|_| "req.send failed")?;
    pw_log::info!("Request sent. Switching to slave mode to ACK response from device B.");

    // Phase 2: switch I2C2 to slave mode so device B's response frame is ACKed
    // on the bus. We drop the Stack (and its server and I2cSender) to release
    // the master driver, then re-wrap the peripheral as a slave.
    //
    // The Stack's Drop impl calls drop_handle on the req channel, which calls
    // server.unbind. That is fine — we have already sent and do not need the
    // handle after this point.
    drop(req);
    drop(stack);

    // SAFETY: master operations above are complete; I2C2 hardware is re-wrapped
    // here exclusively as a slave.
    let mut slave = unsafe {
        let mmio =
            Ast1060I2cRegisters::new(ast1060_pac::I2c2::ptr(), ast1060_pac::I2cbuff2::ptr());
        Ast1060I2c::new(mmio, &i2c2_config(), |_| core::hint::spin_loop())
    }
    .map_err(|_| "I2C2 slave re-init failed")?;

    let slave_cfg =
        SlaveConfig::new(OWN_I2C_ADDR).map_err(|_| "SlaveConfig::new failed")?;
    slave
        .configure_slave(&slave_cfg)
        .map_err(|_| "configure_slave failed")?;

    // Wait for device B to send the response frame. We ACK it at the hardware
    // level but do not decode the payload — that is tested in the req_recv test.
    match wait_data_received(&mut slave, 100_000_000) {
        Some(SlaveEvent::DataReceived { .. }) => {
            pw_log::info!("Response frame received and ACKed from device B");
        }
        Some(_) => return Err("unexpected slave event waiting for response"),
        None => return Err("timed out waiting for response from device B"),
    }

    Ok(())
}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 MCTP Resp-Channel Sender";

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
