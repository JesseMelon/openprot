// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! MCTP response-channel test — receiver side (device B)
//!
//! Runs on the AST1060 Test Harness board with J15 pins 1 and 2 connected.
//! This binary is the RECEIVER (I2C slave). Load it on device B before loading
//! the sender image on device A.
//!
//! Receives an MCTP request from device A through the transport layer,
//! retrieves it via MctpListener::recv, then sends a response back to device A
//! using the MctpRespChannel returned by recv.
//!
//! API surface exercised:
//!   Stack::new, Stack::listener → StackListener
//!   MctpListener::recv → (meta, payload, resp)   ← StackRespChannel returned
//!   MctpRespChannel::send(RESPONSE_PAYLOAD)       ← the new surface under test
//!     → StackRespChannel::send → MctpClient::send(handle=None, eid=Some, tag=Some)
//!     → DirectClient::send → Server::send(handle=None) → Tag::Unowned → LazyI2cSender
//!     → I2C master write back to device A at 0x10

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
use openprot_mctp_api::{Handle, MctpClient, MctpError, MctpListener, MctpRespChannel, RecvMetadata, ResponseCode};
use openprot_mctp_api::stack::Stack;
use openprot_mctp_transport_i2c::{I2cSender, MctpI2cReceiver};
use target_common::{TargetInterface, declare_target};

pub struct Target {}

const SLAVE_ADDR: u8        = 0x42;
const MASTER_ADDR: u8       = 0x10;
const OWN_EID: u8           = 9;
const MSG_TYPE: u8           = 1;
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
            // WriteRequest and Stop are normal sequencing events in a write
            // transaction; keep polling past them.
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
// the full slave receive cycle internally; send_vectored on the LazyI2cSender
// handles master-mode I2C for resp.send().
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

    fn get_eid(&self) -> u8 {
        self.server_mut().get_eid()
    }

    fn set_eid(&self, eid: u8) -> Result<(), MctpError> {
        self.server_mut().set_eid(eid)
    }

    fn recv(
        &self,
        handle: Handle,
        _timeout_millis: u32,
        buf: &mut [u8],
    ) -> Result<RecvMetadata, MctpError> {
        let internal_err = MctpError::from_code(ResponseCode::InternalError);

        // Init I2C as slave and receive the request frame from device A.
        // SAFETY: I2C2 MMIO is not in use by any other owner at this point.
        let mut slave = unsafe {
            let mmio = Ast1060I2cRegisters::new(
                ast1060_pac::I2c2::ptr(),
                ast1060_pac::I2cbuff2::ptr(),
            );
            Ast1060I2c::new(mmio, &i2c2_config(), yield_fn as fn(u32))
        }
        .map_err(|_| internal_err)?;

        pw_log::info!(
            "Listening at addr 0x{:02x}. Start sender (device A) now.",
            self.own_i2c_addr as u32
        );

        let slave_cfg = SlaveConfig::new(self.own_i2c_addr).map_err(|_| internal_err)?;
        slave.configure_slave(&slave_cfg).map_err(|_| internal_err)?;

        let raw_pkt = match wait_data_received(&mut slave, 100_000_000) {
            Some(SlaveEvent::DataReceived { len }) => {
                let mut raw = [0u8; 128];
                let n = len.min(raw.len());
                slave.slave_read(&mut raw[..n]).map_err(|_| internal_err)?;

                // BufferMode delivers [cmd][bc][src][mctp_hdr...][pec] without the
                // leading dest-addr byte. Prepend it so decode sees the full frame.
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

        pw_log::info!("Request received. Switching to master mode to send response.");

        // Route the received packet and spin until try_recv delivers it to the handle.
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
        self.server_mut()
            .send(handle, msg_type, eid, tag, integrity_check, buf)
    }

    fn drop_handle(&self, handle: Handle) {
        let _ = self.server_mut().unbind(handle);
    }
}

// ---------------------------------------------------------------------------

fn run_receiver() -> Result<(), &'static str> {
    pw_log::info!("=== MCTP resp-channel test: RECEIVER (device B) ===");

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
            LazyI2cSender { own_i2c_addr: SLAVE_ADDR, remote_i2c_addr: MASTER_ADDR },
        );
    let client = DirectClient::new(server, SLAVE_ADDR);
    let stack = Stack::new(client);

    // Register the listener before recv() is called so the router has a
    // delivery target when inbound() runs inside the transport layer.
    let mut listener = stack
        .listener(MSG_TYPE, 0)
        .map_err(|_| "stack.listener failed")?;

    let mut buf = [0u8; 128];
    let (_meta, _payload, mut resp) =
        listener.recv(&mut buf).map_err(|_| "listener.recv failed")?;

    // Send the response. StackRespChannel::send calls:
    //   MctpClient::send(handle=None, msg_type, eid=Some(remote_eid),
    //                    tag=Some(msg_tag), ic=false, RESPONSE_PAYLOAD)
    // Server::send sees handle=None and uses Tag::Unowned(msg_tag), which
    // tells the router this is a response, not a new request. The router
    // builds the MCTP packet with the TO (tag-owner) bit clear, then
    // LazyI2cSender creates a fresh master-mode I2cSender and writes to
    // MASTER_ADDR (0x10 = device A).
    pw_log::info!("Sending response via MctpRespChannel::send");
    resp.send(RESPONSE_PAYLOAD).map_err(|_| "resp.send failed")?;

    pw_log::info!("Response sent successfully");
    Ok(())
}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 MCTP Resp-Channel Receiver";

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
