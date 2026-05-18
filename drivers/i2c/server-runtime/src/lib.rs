// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! IPC dispatch loop. **One IPC channel per bus** — that is the entire
//! multi-bus story: a client process is wired by configuration to exactly one
//! bus's channel and physically cannot address another. The server owns one
//! driver per bus and routes each wake-up to its bus by channel handle.
//!
//! Topology-agnostic, like the usart server's loop: the runtime takes a list
//! of `(channel, driver)` bindings, registers each channel with the
//! WaitGroup, and routes wake-ups via `user_data`. Adding another bus is one
//! more entry in the slice — no code change here.
//!
//! There is no IRQ branch and no deferred/parked request: every i2c
//! `Transaction` runs to completion within a single `dispatch` call.
//!
//! The **only** kernel-tagged server crate: it wraps the host-buildable
//! `i2c_server::dispatch` in the Pigweed wait/respond loop.

#![no_std]

use i2c_api::seam::{I2c, SevenBitAddress};
use i2c_server::{dispatch, MAX_BUF_SIZE};
use userspace::syscall::{self, Signals};
use userspace::time::Instant;

/// One bus the server owns: the IPC channel its (single) client uses, and the
/// driver instance for that physical controller.
pub struct Bus<B> {
    /// IPC channel handle (`channel_handler` object) dedicated to this bus.
    pub channel: u32,
    /// The bus driver — any `embedded_hal::i2c::I2c` master implementation.
    pub driver: B,
}

impl<B> Bus<B> {
    pub const fn new(channel: u32, driver: B) -> Self {
        Self { channel, driver }
    }
}

/// Run the i2c server forever.
///
/// Registers every bus's channel with `wg` (`user_data = channel`), then loops
/// waiting on the group. Each readable wake-up is read, dispatched on that
/// bus's driver, and answered on the same channel.
///
/// `buses` must be non-empty and channel handles must be distinct.
pub fn run<B>(wg: u32, buses: &mut [Bus<B>]) -> !
where
    B: I2c<SevenBitAddress>,
{
    for bus in buses.iter() {
        // user_data = the channel handle itself ⇒ routing is a handle compare.
        let _ = syscall::wait_group_add(
            wg,
            bus.channel,
            Signals::READABLE,
            bus.channel as usize,
        );
    }

    let mut request_buf = [0u8; MAX_BUF_SIZE];
    let mut response_buf = [0u8; MAX_BUF_SIZE];

    loop {
        let Ok(wait_return) = syscall::object_wait(wg, Signals::READABLE, Instant::MAX) else {
            continue;
        };
        if !wait_return.pending_signals.contains(Signals::READABLE) {
            continue;
        }

        let channel = wait_return.user_data as u32;
        let Some(bus) = buses.iter_mut().find(|b| b.channel == channel) else {
            continue;
        };

        let Ok(req_len) = syscall::channel_read(channel, 0, &mut request_buf) else {
            continue;
        };

        let resp_len = dispatch(&mut bus.driver, &request_buf[..req_len], &mut response_buf);
        let _ = syscall::channel_respond(channel, &response_buf[..resp_len]);
    }
}
