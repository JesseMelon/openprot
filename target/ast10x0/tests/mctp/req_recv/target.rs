// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! MCTP req-recv test — sender side (device A)
//!
//! Runs on the AST1060 Test Harness board with J15 pins 1 and 2 connected,
//! which links I2C2 between device A and device B.
//!
//! This binary is the SENDER (I2C master). Load the companion receiver binary
//! on device B first, then load this image on device A.
//!
//! Sends one MCTP request, then calls req.recv() which internally switches to
//! slave mode to receive device B's response frame, decodes it through the
//! transport layer, and routes it to the req handle.
//!
//! API surface exercised:
//!   Stack::new, Stack::req → MctpReqChannel
//!   MctpReqChannel::send  (request path: handle=Some, Tag::Owned)
//!   MctpReqChannel::recv  ← the new surface under test
//!     → StackReqChannel::recv → MctpClient::recv → DirectClient::recv
//!     → slave receive → inbound → Server::try_recv(handle, buf) → payload validation
//!
//! Peripheral lifecycle on device A (all managed inside DirectClient::recv):
//!   Phase 1 — LazyI2cSender creates master-mode I2C hardware on demand for
//!              req.send(), then drops it.
//!   Phase 2 — DirectClient::recv creates slave-mode I2C to receive device B's
//!              response frame.
//!   Phase 3 — slave I2C dropped; inbound() routes the response to the req handle.

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
use openprot_mctp_api::{Handle, MctpClient, MctpError, MctpReqChannel, RecvMetadata, ResponseCode};
use openprot_mctp_api::stack::Stack;
use openprot_mctp_transport_i2c::{I2cSender, MctpI2cReceiver};
use target_common::{TargetInterface, declare_target};

pub struct Target {}

const OWN_I2C_ADDR: u8        = 0x10;
const SLAVE_I2C_ADDR: u8      = 0x42;
const OWN_EID: u8             = 8;
const REMOTE_EID: u8          = 9;
const MSG_TYPE: u8             = 1;
const REQUEST_PAYLOAD: &[u8]  = b"mctp_req_hw";
const RESPONSE_PAYLOAD: &[u8] = b"mctp_resp_hw";

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

fn yield_fn(_: u32) { core::hint::spin_loop() }

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

// ---------------------------------------------------------------------------
// LazyI2cSender — mctp_lib::Sender that creates master-mode I2C hardware on
// demand inside send_vectored, then drops it.  No lifetime parameters.
// ---------------------------------------------------------------------------

struct LazyI2cSender {
    own_i2c_addr: u8,
    remote_i2c_addr: u8,
}

impl mctp_lib::Sender for LazyI2cSender {
    fn send_vectored(
        &mut self,
        fragmenter: mctp_lib::fragment::Fragmenter,
        payload: &[&[u8]],
    ) -> mctp::Result<mctp::Tag> {
        // SAFETY: I2C2 MMIO is not in use by any other owner at this point.
        let i2c = unsafe {
            let mmio = Ast1060I2cRegisters::new(
                ast1060_pac::I2c2::ptr(),
                ast1060_pac::I2cbuff2::ptr(),
            );
            Ast1060I2c::new(mmio, &i2c2_config(), yield_fn as fn(u32)).unwrap()
        };
        I2cSender::new(i2c, self.own_i2c_addr, self.remote_i2c_addr)
            .send_vectored(fragmenter, payload)
    }

    fn get_mtu(&self) -> usize {
        mctp_lib::i2c::MCTP_I2C_MAXMTU
    }
}

// ---------------------------------------------------------------------------
// DirectClient — MctpClient backed by Server<LazyI2cSender>.  recv() drives
// the full slave receive cycle: creates slave-mode I2C, receives and decodes
// device B's response, routes it via inbound(), then spins on try_recv.
// ---------------------------------------------------------------------------

struct DirectClient<const N: usize> {
    server: core::cell::UnsafeCell<openprot_mctp_server::Server<LazyI2cSender, N>>,
    own_i2c_addr: u8,
    receiver: MctpI2cReceiver,
}

impl<const N: usize> DirectClient<N> {
    fn new(
        server: openprot_mctp_server::Server<LazyI2cSender, N>,
        own_i2c_addr: u8,
    ) -> Self {
        DirectClient {
            server: core::cell::UnsafeCell::new(server),
            own_i2c_addr,
            receiver: MctpI2cReceiver::new(own_i2c_addr),
        }
    }

    fn server_mut(&self) -> &mut openprot_mctp_server::Server<LazyI2cSender, N> {
        // SAFETY: single-threaded bare-metal; no concurrent access.
        unsafe { &mut *self.server.get() }
    }
}

impl<const N: usize> MctpClient for DirectClient<N> {
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
        let internal_err = MctpError::from_code(ResponseCode::InternalError);

        pw_log::info!("Request sent. Switching to slave mode to receive response from device B.");

        // Phase 2: create slave-mode I2C to receive device B's response.
        // SAFETY: LazyI2cSender's send_vectored has completed and dropped the
        // master-mode Ast1060I2c; I2C2 MMIO is exclusively used through `slave`.
        let mut slave = unsafe {
            let mmio = Ast1060I2cRegisters::new(
                ast1060_pac::I2c2::ptr(),
                ast1060_pac::I2cbuff2::ptr(),
            );
            Ast1060I2c::new(mmio, &i2c2_config(), yield_fn as fn(u32))
        }
        .map_err(|_| internal_err)?;

        let slave_cfg = SlaveConfig::new(self.own_i2c_addr).map_err(|_| internal_err)?;
        slave.configure_slave(&slave_cfg).map_err(|_| internal_err)?;

        // Receive and decode device B's response frame.
        let raw_pkt = match wait_data_received(&mut slave, 100_000_000) {
            Some(SlaveEvent::DataReceived { len }) => {
                let mut raw = [0u8; 128];
                let n = len.min(raw.len());
                slave.slave_read(&mut raw[..n]).map_err(|_| internal_err)?;

                // BufferMode omits the leading dest-addr byte; prepend it for decode.
                let mut frame = [0u8; 129];
                frame[0] = self.own_i2c_addr << 1;
                frame[1..n + 1].copy_from_slice(&raw[..n]);

                let (pkt, _) =
                    self.receiver.decode(&frame[..n + 1]).map_err(|_| internal_err)?;

                let mut pkt_buf = [0u8; 128];
                let pkt_len = pkt.len().min(pkt_buf.len());
                pkt_buf[..pkt_len].copy_from_slice(&pkt[..pkt_len]);
                (pkt_buf, pkt_len)
            }
            Some(_) => return Err(internal_err),
            None => return Err(MctpError::from_code(ResponseCode::TimedOut)),
        };
        drop(slave);

        pw_log::info!("Response frame received. Restoring master sender and calling req.recv().");

        // Phase 3: route the response packet and spin until try_recv delivers it.
        let (pkt_buf, pkt_len) = raw_pkt;
        self.server_mut()
            .inbound(&pkt_buf[..pkt_len])
            .map_err(|_| internal_err)?;

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

fn run_sender() -> Result<(), &'static str> {
    pw_log::info!("=== MCTP req-recv test: SENDER (device A) ===");
    pw_log::info!("J15 must be connected. Load receiver image on device B first.");

    let board = Ast10x0Board::new(Ast10x0BoardDescriptor {
        pinctrl_groups: &[pinctrl::PINCTRL_I2C2],
        i2c_buses: &[],
    });
    // SAFETY: single call at boot with exclusive access to SCU/I2C global regs.
    unsafe { board.init() }.map_err(|_| "board init failed")?;

    let server: openprot_mctp_server::Server<LazyI2cSender, 4> =
        openprot_mctp_server::Server::new(
            mctp::Eid(OWN_EID),
            0,
            LazyI2cSender { own_i2c_addr: OWN_I2C_ADDR, remote_i2c_addr: SLAVE_I2C_ADDR },
        );
    let client = DirectClient::new(server, OWN_I2C_ADDR);
    let stack = Stack::new(client);

    pw_log::info!(
        "Sending MCTP request to EID {} via I2C 0x{:02x}",
        REMOTE_EID as u32,
        SLAVE_I2C_ADDR as u32,
    );
    let mut req = stack.req(REMOTE_EID, 0).map_err(|_| "stack.req failed")?;
    req.send(MSG_TYPE, REQUEST_PAYLOAD).map_err(|_| "req.send failed")?;

    // req.recv() drives the full slave receive cycle internally: switches to
    // slave mode, receives and decodes device B's response, routes the packet
    // through the server, and returns the payload.
    let mut buf = [0u8; 128];
    let (_meta, payload) = req.recv(&mut buf).map_err(|_| "req.recv failed")?;

    if payload != RESPONSE_PAYLOAD {
        pw_log::error!("Payload mismatch");
        return Err("response payload mismatch");
    }

    pw_log::info!("req.recv() returned correct payload — PASS");
    Ok(())
}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 MCTP Req-Recv Sender";

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
