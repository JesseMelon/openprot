// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Wire protocol for the i2c userspace driver.
//!
//! The unit marshalled across the IPC boundary is **one whole
//! `embedded_hal::i2c::I2c::transaction`** — an address plus an ordered list
//! of `Read`/`Write` operations the server replays atomically on the real bus
//! before replying. There is no per-operation round-trip and no bus
//! lock/unlock exposed across the boundary; that is what preserves the
//! exclusive-atomic contract of `I2c` between processes.
//!
//! ```text
//! request : [I2cRequestHeader] [I2cOpDesc; op_count] [write payloads...]
//! response: [I2cResponseHeader] [read payloads...]
//! ```
//!
//! `Write` ops carry their bytes inline (in operation order) after the
//! descriptor array. `Read` ops contribute nothing to the request; their
//! bytes come back, concatenated in operation order, after the response
//! header and are scattered into the caller's slices by the client.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Max total payload (sum of read or write bytes) in one transaction.
pub const MAX_PAYLOAD_SIZE: usize = 256;

/// Max number of `Operation`s in one transaction.
pub const MAX_OPS: usize = 16;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cOp {
    /// One whole `I2c::transaction(address, &mut [Operation])`.
    Transaction = 0x01,
}

impl TryFrom<u8> for I2cOp {
    type Error = I2cError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Transaction),
            _ => Err(I2cError::InvalidOperation),
        }
    }
}

/// Kind of a single bus operation within a transaction.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cOpKind {
    Write = 0x00,
    Read = 0x01,
}

impl TryFrom<u8> for I2cOpKind {
    type Error = I2cError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::Write),
            0x01 => Ok(Self::Read),
            _ => Err(I2cError::InvalidOperation),
        }
    }
}

/// Status / error code carried in `I2cResponseHeader`.
///
/// The bus-failure variants map 1-to-1 onto `embedded_hal::i2c::ErrorKind`
/// (see [`crate::seam::error_kind`]).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cError {
    Success = 0x00,
    InvalidOperation = 0x01,
    BufferTooSmall = 0x02,
    TooManyOperations = 0x03,
    /// NACK on the address phase.
    AddressNack = 0x04,
    /// NACK on a data byte.
    DataNack = 0x05,
    /// NACK, phase unknown.
    Nack = 0x06,
    ArbitrationLoss = 0x07,
    Bus = 0x08,
    Overrun = 0x09,
    Timeout = 0x0A,
    InternalError = 0xFF,
}

impl From<u8> for I2cError {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::Success,
            0x01 => Self::InvalidOperation,
            0x02 => Self::BufferTooSmall,
            0x03 => Self::TooManyOperations,
            0x04 => Self::AddressNack,
            0x05 => Self::DataNack,
            0x06 => Self::Nack,
            0x07 => Self::ArbitrationLoss,
            0x08 => Self::Bus,
            0x09 => Self::Overrun,
            0x0A => Self::Timeout,
            _ => Self::InternalError,
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct I2cRequestHeader {
    pub op_code: u8,
    pub flags: u8,
    /// Target address. 7-bit address in the low 7 bits; 10-bit reserved.
    pub address: u16,
    /// Number of `I2cOpDesc` records that follow this header.
    pub op_count: u16,
    /// Total bytes after the header (op descriptors + inline write data).
    pub payload_len: u16,
}

impl I2cRequestHeader {
    pub const SIZE: usize = 8;

    pub fn new(op: I2cOp, address: u16, op_count: u16, payload_len: u16) -> Self {
        Self {
            op_code: op as u8,
            flags: 0,
            address: address.to_le(),
            op_count: op_count.to_le(),
            payload_len: payload_len.to_le(),
        }
    }

    pub fn operation(&self) -> Result<I2cOp, I2cError> {
        I2cOp::try_from(self.op_code)
    }

    pub fn address_value(&self) -> u16 {
        u16::from_le(self.address)
    }

    pub fn op_count_value(&self) -> usize {
        u16::from_le(self.op_count) as usize
    }

    pub fn payload_length(&self) -> usize {
        u16::from_le(self.payload_len) as usize
    }
}

/// One operation descriptor in the request body. For `Write`, `len` bytes of
/// inline payload follow (concatenated across ops, in order); for `Read`,
/// `len` is the number of bytes expected back in the response payload.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct I2cOpDesc {
    pub kind: u8,
    pub reserved: u8,
    pub len: u16,
}

impl I2cOpDesc {
    pub const SIZE: usize = 4;

    pub fn new(kind: I2cOpKind, len: u16) -> Self {
        Self {
            kind: kind as u8,
            reserved: 0,
            len: len.to_le(),
        }
    }

    pub fn op_kind(&self) -> Result<I2cOpKind, I2cError> {
        I2cOpKind::try_from(self.kind)
    }

    pub fn length(&self) -> usize {
        u16::from_le(self.len) as usize
    }
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct I2cResponseHeader {
    pub status: u8,
    pub reserved: u8,
    /// Total read-payload bytes following this header.
    pub payload_len: u16,
}

impl I2cResponseHeader {
    pub const SIZE: usize = 4;

    pub fn success(payload_len: u16) -> Self {
        Self {
            status: I2cError::Success as u8,
            reserved: 0,
            payload_len: payload_len.to_le(),
        }
    }

    pub fn error(error: I2cError) -> Self {
        Self {
            status: error as u8,
            reserved: 0,
            payload_len: 0,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == I2cError::Success as u8
    }

    pub fn error_code(&self) -> I2cError {
        I2cError::from(self.status)
    }

    pub fn payload_length(&self) -> usize {
        u16::from_le(self.payload_len) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::{FromBytes, IntoBytes};

    #[test]
    fn request_header_roundtrips_through_bytes() {
        let h = I2cRequestHeader::new(I2cOp::Transaction, 0x48, 3, 40);
        let bytes = h.as_bytes();
        assert_eq!(bytes.len(), I2cRequestHeader::SIZE);
        let decoded = I2cRequestHeader::ref_from_bytes(bytes).unwrap();
        assert_eq!(decoded.operation(), Ok(I2cOp::Transaction));
        assert_eq!(decoded.address_value(), 0x48);
        assert_eq!(decoded.op_count_value(), 3);
        assert_eq!(decoded.payload_length(), 40);
    }

    #[test]
    fn op_desc_roundtrips_and_decodes_kind() {
        for (kind, raw) in [(I2cOpKind::Write, 0u8), (I2cOpKind::Read, 1u8)] {
            let d = I2cOpDesc::new(kind, 17);
            let bytes = d.as_bytes();
            assert_eq!(bytes.len(), I2cOpDesc::SIZE);
            let decoded = I2cOpDesc::ref_from_bytes(bytes).unwrap();
            assert_eq!(decoded.length(), 17);
            assert_eq!(decoded.kind, raw);
            assert_eq!(decoded.op_kind(), Ok(kind));
        }
    }

    #[test]
    fn response_header_success_and_error() {
        let ok = I2cResponseHeader::success(12);
        let ok = I2cResponseHeader::ref_from_bytes(ok.as_bytes()).unwrap();
        assert!(ok.is_success());
        assert_eq!(ok.payload_length(), 12);

        let err = I2cResponseHeader::error(I2cError::AddressNack);
        let err = I2cResponseHeader::ref_from_bytes(err.as_bytes()).unwrap();
        assert!(!err.is_success());
        assert_eq!(err.error_code(), I2cError::AddressNack);
        assert_eq!(err.payload_length(), 0);
    }

    #[test]
    fn error_and_op_byte_mapping_is_stable() {
        for raw in 0u8..=0x0A {
            assert_eq!(I2cError::from(raw) as u8, raw);
        }
        assert_eq!(I2cError::from(0xFF), I2cError::InternalError);
        assert_eq!(I2cError::from(0x42), I2cError::InternalError);
        assert_eq!(I2cOp::try_from(0x01), Ok(I2cOp::Transaction));
        assert_eq!(I2cOp::try_from(0x99), Err(I2cError::InvalidOperation));
    }
}
