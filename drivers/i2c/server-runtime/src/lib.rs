// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! IPC dispatch loop. **One IPC channel per bus** — a client process is wired
//! by configuration to exactly one bus's channel and physically cannot
//! address another. The server owns one driver per bus (master *and* slave)
//! and routes each wake-up to its bus by channel handle.
//!
//! Topology-agnostic, like the usart server's loop: the runtime takes a list
//! of bus bindings + the i2c IRQ, registers them with the WaitGroup, and
//! routes wake-ups via `user_data`.
//!
//! ## Slave-receive notification (thin slice)
//!
//! The WaitGroup multiplexes the per-bus client channels (`READABLE`) **and**
//! the i2c hardware IRQ. On the IRQ: for every notification-armed bus, drain
//! the slave RX into that bus's latch, `interrupt_ack`, then raise
//! `Signals::USER` on that bus's client channel. The client then issues
//! `SlaveReceive`, which returns the latched bytes (status `NoData` if empty).
//! Arm/disarm is `EnableSlaveNotification` / `DisableSlaveNotification`.
//!
//! The **only** kernel-tagged server crate; it wraps the host-buildable
//! `i2c_server::{dispatch, slave::dispatch_slave}` in the Pigweed loop.

#![no_std]

use i2c_api::seam::{I2c, I2cSlaveBuffer, SevenBitAddress};
use i2c_api::{I2cError, I2cOp, I2cRequestHeader, I2cResponseHeader, MAX_PAYLOAD_SIZE};
use i2c_server::slave::dispatch_slave;
use i2c_server::{dispatch, MAX_BUF_SIZE};
use userspace::syscall::{self, Signals};
use userspace::time::Instant;

/// One bus the server owns: its dedicated IPC channel, the driver instance
/// (master + slave), and the per-bus slave-RX notification latch.
pub struct Bus<B> {
    /// IPC channel handle (`channel_handler`) dedicated to this bus.
    pub channel: u32,
    /// The bus driver — implements both the master and slave seams.
    pub driver: B,
    notif_enabled: bool,
    rx: [u8; MAX_PAYLOAD_SIZE],
    rx_len: usize,
}

impl<B> Bus<B> {
    pub const fn new(channel: u32, driver: B) -> Self {
        Self {
            channel,
            driver,
            notif_enabled: false,
            rx: [0u8; MAX_PAYLOAD_SIZE],
            rx_len: 0,
        }
    }
}

fn encode_error(resp: &mut [u8], e: I2cError) -> usize {
    let h = I2cResponseHeader::error(e);
    resp[..I2cResponseHeader::SIZE].copy_from_slice(zerocopy::IntoBytes::as_bytes(&h));
    I2cResponseHeader::SIZE
}

fn encode_ok(resp: &mut [u8], payload_len: usize) -> usize {
    let h = I2cResponseHeader::success(payload_len as u16);
    resp[..I2cResponseHeader::SIZE].copy_from_slice(zerocopy::IntoBytes::as_bytes(&h));
    I2cResponseHeader::SIZE + payload_len
}

/// Parse just the request header (op + the SlaveReceive max-len in op_count).
fn header(req: &[u8]) -> Option<(I2cOp, usize)> {
    if req.len() < I2cRequestHeader::SIZE {
        return None;
    }
    let h = zerocopy::Ref::<_, I2cRequestHeader>::from_bytes(&req[..I2cRequestHeader::SIZE]).ok()?;
    Some((h.operation().ok()?, h.op_count_value()))
}

/// Run the i2c server forever.
///
/// Registers every bus channel (`READABLE`) and the i2c `irq` (`irq_signals`)
/// with `wg`, then loops. `buses` must be non-empty with distinct channels.
pub fn run<B>(wg: u32, irq: u32, irq_signals: Signals, buses: &mut [Bus<B>]) -> !
where
    B: I2c<SevenBitAddress> + I2cSlaveBuffer<SevenBitAddress>,
{
    for bus in buses.iter() {
        let _ = syscall::wait_group_add(wg, bus.channel, Signals::READABLE, bus.channel as usize);
    }
    let _ = syscall::wait_group_add(wg, irq, irq_signals, irq as usize);

    let mut request_buf = [0u8; MAX_BUF_SIZE];
    let mut response_buf = [0u8; MAX_BUF_SIZE];
    let wait_mask = Signals::READABLE | irq_signals;

    loop {
        let Ok(w) = syscall::object_wait(wg, wait_mask, Instant::MAX) else {
            continue;
        };

        // ---- hardware slave IRQ: drain armed buses, ack, wake clients ----
        if w.user_data as u32 == irq && w.pending_signals.contains(irq_signals) {
            let acked = w.pending_signals & irq_signals;
            for bus in buses.iter_mut() {
                if !bus.notif_enabled {
                    continue;
                }
                if let Ok(Some(_)) = bus.driver.poll_slave_data() {
                    if let Ok(n) = bus.driver.read_slave_buffer(&mut bus.rx) {
                        if n > 0 {
                            bus.rx_len = n;
                        }
                    }
                }
            }
            let _ = syscall::interrupt_ack(irq, acked);
            for bus in buses.iter() {
                if bus.notif_enabled && bus.rx_len > 0 {
                    // ORs USER onto the bus channel without disturbing READABLE.
                    let _ = syscall::object_set_peer_user_signal(bus.channel, true);
                }
            }
            continue;
        }

        if !w.pending_signals.contains(Signals::READABLE) {
            continue;
        }
        let channel = w.user_data as u32;
        let Some(bus) = buses.iter_mut().find(|b| b.channel == channel) else {
            continue;
        };
        let Ok(req_len) = syscall::channel_read(channel, 0, &mut request_buf) else {
            continue;
        };
        let req = &request_buf[..req_len];

        let resp_len = match header(req) {
            Some((I2cOp::Transaction, _)) => dispatch(&mut bus.driver, req, &mut response_buf),
            Some((I2cOp::ConfigureSlave | I2cOp::EnableSlave | I2cOp::DisableSlave, _)) => {
                dispatch_slave(&mut bus.driver, req, &mut response_buf)
            }
            Some((I2cOp::EnableSlaveNotification, _)) => {
                bus.notif_enabled = true;
                encode_ok(&mut response_buf, 0)
            }
            Some((I2cOp::DisableSlaveNotification, _)) => {
                bus.notif_enabled = false;
                bus.rx_len = 0;
                encode_ok(&mut response_buf, 0)
            }
            Some((I2cOp::SlaveReceive, max_len)) => {
                if bus.rx_len == 0 {
                    encode_error(&mut response_buf, I2cError::NoData)
                } else {
                    let cap = response_buf.len() - I2cResponseHeader::SIZE;
                    let n = bus.rx_len.min(max_len).min(cap);
                    response_buf[I2cResponseHeader::SIZE..I2cResponseHeader::SIZE + n]
                        .copy_from_slice(&bus.rx[..n]);
                    bus.rx_len = 0;
                    encode_ok(&mut response_buf, n)
                }
            }
            None => encode_error(&mut response_buf, I2cError::InvalidOperation),
        };
        let _ = syscall::channel_respond(channel, &response_buf[..resp_len]);
    }
}
