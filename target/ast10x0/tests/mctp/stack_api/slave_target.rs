// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! MCTP stack API test — receiver side (device B)
//!
//! Runs on the AST1060 Test Harness board with J15 pins 1 and 2 connected.
//! This binary is the RECEIVER (I2C slave). Load it on device B before loading
//! the sender image on device A.
//!
//! Receives an MCTP-over-I2C frame, feeds it through `Server::inbound()`,
//! then retrieves the message through `Stack` / `MctpListener` — the
//! application-facing API layer above the raw server dispatch methods.

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
use openprot_mctp_api::{Handle, MctpClient, MctpError, MctpListener, RecvMetadata};
use openprot_mctp_api::stack::Stack;
use openprot_mctp_transport_i2c::{I2cSender, MctpI2cReceiver};
use target_common::{TargetInterface, declare_target};

pub struct Target {}

const SLAVE_ADDR: u8 = 0x42;
const MASTER_ADDR: u8 = 0x10;
const OWN_EID: u8 = 9;
const MSG_TYPE: u8 = 1;
const EXPECTED_PAYLOAD: &[u8] = b"mctp_api_hw";

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

// ---------------------------------------------------------------------------
// DirectClient: MctpClient backed directly by openprot_mctp_server::Server
// ---------------------------------------------------------------------------

struct DirectClient<S: mctp_lib::Sender, const N: usize> {
    server: core::cell::UnsafeCell<openprot_mctp_server::Server<S, N>>,
}

impl<S: mctp_lib::Sender, const N: usize> DirectClient<S, N> {
    fn new(server: openprot_mctp_server::Server<S, N>) -> Self {
        DirectClient {
            server: core::cell::UnsafeCell::new(server),
        }
    }

    fn server_mut(&self) -> &mut openprot_mctp_server::Server<S, N> {
        // SAFETY: single-threaded bare-metal; no concurrent access.
        unsafe { &mut *self.server.get() }
    }

    fn inbound(&self, pkt: &[u8]) -> Result<(), MctpError> {
        self.server_mut().inbound(pkt)
    }
}

impl<S: mctp_lib::Sender, const N: usize> MctpClient for DirectClient<S, N> {
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
        // Spin until the server has a message routed to this handle.
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
    pw_log::info!("=== MCTP stack API test: RECEIVER (device B) ===");
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

    let i2c_receiver = MctpI2cReceiver::new(SLAVE_ADDR);

    let raw_pkt = match wait_data_received(&mut slave, 100_000_000) {
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

            let (pkt, _) = i2c_receiver
                .decode(&frame[..n + 1])
                .map_err(|_| "MCTP decode failed")?;

            let mut pkt_buf = [0u8; 128];
            let pkt_len = pkt.len().min(pkt_buf.len());
            pkt_buf[..pkt_len].copy_from_slice(&pkt[..pkt_len]);
            (pkt_buf, pkt_len)
        }
        Some(_) => return Err("unexpected slave event"),
        None => return Err("timed out waiting for MCTP frame"),
    };

    // Slave receive is complete. Re-wrap the I2C hardware for the server's sender.
    //
    // SAFETY: slave operations above are complete; I2C2 hardware is re-wrapped here exclusively.
    let i2c = unsafe {
        let mmio =
            Ast1060I2cRegisters::new(ast1060_pac::I2c2::ptr(), ast1060_pac::I2cbuff2::ptr());
        Ast1060I2c::new(mmio, &i2c2_config(), |_| core::hint::spin_loop())
    }
    .map_err(|_| "I2C2 re-init failed")?;
    drop(slave);

    let i2c_sender = I2cSender::new(i2c, SLAVE_ADDR, MASTER_ADDR);
    let server: openprot_mctp_server::Server<_, 4> =
        openprot_mctp_server::Server::new(mctp::Eid(OWN_EID), 0, i2c_sender);

    let client = DirectClient::new(server);
    let stack = Stack::new(client);

    // Register the listener before feeding the packet so the router has a
    // destination to deliver the message to.
    let mut listener = stack
        .listener(MSG_TYPE, 0)
        .map_err(|_| "stack.listener failed")?;

    // Feed the decoded MCTP packet into the server's routing layer.
    // The listener is already registered above, so the router delivers it immediately.
    let (pkt_buf, pkt_len) = raw_pkt;
    stack
        .client()
        .inbound(&pkt_buf[..pkt_len])
        .map_err(|_| "inbound failed")?;

    let mut buf = [0u8; 128];
    let (meta, payload, _resp) = listener.recv(&mut buf).map_err(|_| "listener.recv failed")?;

    if meta.msg_type != MSG_TYPE {
        pw_log::error!(
            "msg type: got 0x{:02x}, expected 0x{:02x}",
            meta.msg_type as u32,
            MSG_TYPE as u32
        );
        return Err("message type mismatch");
    }
    if payload != EXPECTED_PAYLOAD {
        return Err("payload mismatch");
    }
    pw_log::info!("MCTP message received via MctpListener and payload verified");

    Ok(())
}

impl TargetInterface for Target {
    const NAME: &'static str = "AST10x0 MCTP Stack API Receiver";

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
