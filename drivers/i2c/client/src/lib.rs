// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Client for the i2c userspace driver.
//!
//! `I2cClient<T>` exposes itself **only** as an implementation of
//! `embedded_hal::i2c::I2c` — consumers depend on that abstract seam, never on
//! the transport. All wire marshalling lives here and is generic over
//! [`Transport`]: the *same* encode/decode code runs in production
//! (`IpcTransport`, cross-process) and in host tests (`LoopbackTransport`,
//! in-process against a mock bus). One `transaction()` call serializes the
//! whole address + operation list, performs exactly one `Transport::transact`,
//! and scatters the read results back into the caller's slices: one seam call
//! ⇒ one round-trip ⇒ one server-side run-to-completion.
//!
//! This crate has **no kernel/IPC dependency** and builds on the host — that
//! is what makes the encoders/decoders testable without a kernel.

#![no_std]

use i2c_api::seam::{error_kind, ErrorKind, ErrorType, I2c, Operation, SevenBitAddress};
use i2c_api::{
    I2cError, I2cOp, I2cOpDesc, I2cOpKind, I2cRequestHeader, I2cResponseHeader, Transport,
    TransportError, MAX_OPS, MAX_PAYLOAD_SIZE,
};

const MAX_BUF_SIZE: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    Transport(TransportError),
    ServerError(I2cError),
    InvalidResponse,
    /// Request or response would exceed `MAX_BUF_SIZE` / `MAX_PAYLOAD_SIZE`.
    /// The whole transaction must fit one round-trip — it is never fragmented.
    BufferTooSmall,
    TooManyOperations,
}

impl From<TransportError> for ClientError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

// Lets a consumer treat `ClientError` through the embedded-hal error taxonomy
// without knowing it came from a userspace driver client.
impl i2c_api::seam::I2cBusError for ClientError {
    fn kind(&self) -> ErrorKind {
        match self {
            ClientError::ServerError(e) => error_kind(*e),
            _ => ErrorKind::Other,
        }
    }
}

/// An i2c client bound to one bus, speaking the `i2c_api` wire protocol over
/// any [`Transport`]. Implements `embedded_hal::i2c::I2c` and nothing else
/// publicly — the transport is invisible to consumers.
pub struct I2cClient<T: Transport> {
    transport: T,
}

impl<T: Transport> I2cClient<T> {
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    // ---- Target/slave mode (thin notification slice) ----
    //
    // One IPC channel per bus, so none of these carry a bus argument — the
    // bus is the channel this client is bound to. Same Transport, same
    // marshalling discipline as master: one whole request, one round-trip.

    /// Send a header-only slave-control op; returns the response payload
    /// length (0 for ack-only ops). `out` receives `SlaveReceive` data.
    fn slave_cmd(
        &mut self,
        op: I2cOp,
        address: u16,
        max_len: u16,
        out: Option<&mut [u8]>,
    ) -> Result<usize, ClientError> {
        let hdr = I2cRequestHeader::new(op, address, max_len, 0);
        let mut req = [0u8; I2cRequestHeader::SIZE];
        req.copy_from_slice(zerocopy::IntoBytes::as_bytes(&hdr));

        let mut resp = [0u8; I2cResponseHeader::SIZE + MAX_PAYLOAD_SIZE];
        let resp_len = self.transport.transact(&req, &mut resp)?;

        if resp_len < I2cResponseHeader::SIZE {
            return Err(ClientError::InvalidResponse);
        }
        let Some(rhdr) =
            zerocopy::Ref::<_, I2cResponseHeader>::from_bytes(&resp[..I2cResponseHeader::SIZE]).ok()
        else {
            return Err(ClientError::InvalidResponse);
        };
        if !rhdr.is_success() {
            return Err(ClientError::ServerError(rhdr.error_code()));
        }
        let n = rhdr.payload_length();
        if resp_len < I2cResponseHeader::SIZE + n {
            return Err(ClientError::InvalidResponse);
        }
        if let Some(buf) = out {
            let copy = n.min(buf.len());
            buf[..copy].copy_from_slice(&resp[I2cResponseHeader::SIZE..I2cResponseHeader::SIZE + copy]);
            return Ok(copy);
        }
        Ok(n)
    }

    /// Set this bus's slave (target) address.
    pub fn configure_slave(&mut self, address: SevenBitAddress) -> Result<(), ClientError> {
        self.slave_cmd(I2cOp::ConfigureSlave, address as u16, 0, None)
            .map(|_| ())
    }

    /// Enter slave mode (start ACKing the configured address).
    pub fn enable_slave(&mut self) -> Result<(), ClientError> {
        self.slave_cmd(I2cOp::EnableSlave, 0, 0, None).map(|_| ())
    }

    /// Leave slave mode.
    pub fn disable_slave(&mut self) -> Result<(), ClientError> {
        self.slave_cmd(I2cOp::DisableSlave, 0, 0, None).map(|_| ())
    }

    /// Arm interrupt-driven slave-RX notification. After this the server
    /// raises `Signals::USER` on this bus's channel when data is latched;
    /// the consumer then calls [`slave_receive`](Self::slave_receive).
    pub fn enable_notification(&mut self) -> Result<(), ClientError> {
        self.slave_cmd(I2cOp::EnableSlaveNotification, 0, 0, None)
            .map(|_| ())
    }

    /// Disarm slave-RX notification (also drops any latched buffer).
    pub fn disable_notification(&mut self) -> Result<(), ClientError> {
        self.slave_cmd(I2cOp::DisableSlaveNotification, 0, 0, None)
            .map(|_| ())
    }

    /// Fetch the latched slave-RX bytes into `buf` (non-blocking). Returns
    /// the byte count, or `Err(ServerError(I2cError::NoData))` if nothing is
    /// pending — call this after a `Signals::USER` wake on the channel.
    pub fn slave_receive(&mut self, buf: &mut [u8]) -> Result<usize, ClientError> {
        let max = buf.len().min(MAX_PAYLOAD_SIZE) as u16;
        self.slave_cmd(I2cOp::SlaveReceive, 0, max, Some(buf))
    }

    fn transact(
        &mut self,
        address: SevenBitAddress,
        operations: &mut [Operation<'_>],
    ) -> Result<(), ClientError> {
        if operations.len() > MAX_OPS {
            return Err(ClientError::TooManyOperations);
        }

        // Size the request: header + one descriptor per op + inline write data.
        // Bound the read side too — the whole transaction must fit one buffer.
        let mut write_total = 0usize;
        let mut read_total = 0usize;
        for op in operations.iter() {
            match op {
                Operation::Write(buf) => write_total += buf.len(),
                Operation::Read(buf) => read_total += buf.len(),
            }
        }

        let desc_bytes = operations.len() * I2cOpDesc::SIZE;
        let req_len = I2cRequestHeader::SIZE + desc_bytes + write_total;
        if req_len > MAX_BUF_SIZE
            || write_total > MAX_PAYLOAD_SIZE
            || read_total > MAX_PAYLOAD_SIZE
            || I2cResponseHeader::SIZE + read_total > MAX_BUF_SIZE
        {
            return Err(ClientError::BufferTooSmall);
        }

        let mut req = [0u8; MAX_BUF_SIZE];
        let mut resp = [0u8; MAX_BUF_SIZE];

        let hdr = I2cRequestHeader::new(
            I2cOp::Transaction,
            address as u16,
            operations.len() as u16,
            (desc_bytes + write_total) as u16,
        );
        req[..I2cRequestHeader::SIZE].copy_from_slice(zerocopy::IntoBytes::as_bytes(&hdr));

        // Descriptor array, in operation order.
        let mut off = I2cRequestHeader::SIZE;
        for op in operations.iter() {
            let desc = match op {
                Operation::Write(buf) => I2cOpDesc::new(I2cOpKind::Write, buf.len() as u16),
                Operation::Read(buf) => I2cOpDesc::new(I2cOpKind::Read, buf.len() as u16),
            };
            req[off..off + I2cOpDesc::SIZE].copy_from_slice(zerocopy::IntoBytes::as_bytes(&desc));
            off += I2cOpDesc::SIZE;
        }

        // Inline write payloads, concatenated in operation order.
        for op in operations.iter() {
            if let Operation::Write(buf) = op {
                req[off..off + buf.len()].copy_from_slice(buf);
                off += buf.len();
            }
        }

        let resp_len = self.transport.transact(&req[..off], &mut resp)?;

        if resp_len < I2cResponseHeader::SIZE {
            return Err(ClientError::InvalidResponse);
        }
        let rhdr_bytes = &resp[..I2cResponseHeader::SIZE];
        let Some(rhdr) = zerocopy::Ref::<_, I2cResponseHeader>::from_bytes(rhdr_bytes).ok() else {
            return Err(ClientError::InvalidResponse);
        };
        if !rhdr.is_success() {
            return Err(ClientError::ServerError(rhdr.error_code()));
        }

        let payload_len = rhdr.payload_length();
        if payload_len != read_total || resp_len < I2cResponseHeader::SIZE + payload_len {
            return Err(ClientError::InvalidResponse);
        }

        // Scatter read results back into the caller's slices, in order.
        let mut rp = I2cResponseHeader::SIZE;
        for op in operations.iter_mut() {
            if let Operation::Read(buf) = op {
                let n = buf.len();
                buf.copy_from_slice(&resp[rp..rp + n]);
                rp += n;
            }
        }

        Ok(())
    }
}

impl<T: Transport> ErrorType for I2cClient<T> {
    type Error = ClientError;
}

impl<T: Transport> I2c<SevenBitAddress> for I2cClient<T> {
    fn transaction(
        &mut self,
        address: SevenBitAddress,
        operations: &mut [Operation<'_>],
    ) -> Result<(), Self::Error> {
        self.transact(address, operations)
    }
}
