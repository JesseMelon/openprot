// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

#![no_main]
#![no_std]

use openprot_mctp_api::Stack;
use openprot_mctp_client_ipc::IpcMctpClient;
use openprot_mctp_echo::prepare_listener_with_eid_and_timeout;
use userspace::{entry, syscall};

const ECHO_EID: u8 = 8;
const LISTEN_TIMEOUT_MS: u32 = 100;

#[entry]
fn entry() {
    let stack = Stack::new(IpcMctpClient::new(app_mctp_echo_client::handle::MCTP));
    let _ = prepare_listener_with_eid_and_timeout(&stack, ECHO_EID, LISTEN_TIMEOUT_MS);
    let _ = syscall::debug_shutdown(Ok(()));
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
