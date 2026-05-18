// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! The abstract seam.
//!
//! The consumer-facing contract is **`embedded_hal::i2c::I2c`** — the
//! canonical embedded-hal 1.0 bus-master trait. We deliberately do *not*
//! reinvent a bespoke read/write trait: the client implements `I2c`, and the
//! server's backend is any real platform driver that already implements `I2c`
//! (`embedded_hal::i2c::I2c` is byte/slice-oriented, so a runtime-decoded wire
//! request can drive it directly — no typestate-impedance shim needed).
//!
//! Re-exported here so the server and per-target backend crates depend on this
//! one seam definition rather than pinning `embedded-hal` independently.

pub use embedded_hal::i2c::{
    AddressMode, Error as I2cBusError, ErrorKind, ErrorType, I2c, NoAcknowledgeSource, Operation,
    SevenBitAddress, TenBitAddress,
};

// The **target/slave seam** is reused verbatim from `openprot-hal-blocking`
// — same principle as the master seam above: do not reinvent. The
// server-runtime is generic over these (configure address / enable / drain);
// the per-target backend implements them by delegating to the SoC driver.
pub use openprot_hal_blocking::i2c_hardware::slave::{
    I2cSEvent, I2cSlaveBuffer, I2cSlaveCore, I2cSlaveInterrupts, SlaveStatus,
};
pub use openprot_hal_blocking::i2c_hardware::I2cHardwareCore;

use crate::protocol::I2cError;

/// Map a wire status code onto the `embedded_hal::i2c::ErrorKind` taxonomy so
/// the client can satisfy `embedded_hal::i2c::Error`.
pub fn error_kind(err: I2cError) -> ErrorKind {
    match err {
        I2cError::AddressNack => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address),
        I2cError::DataNack => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Data),
        I2cError::Nack => ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown),
        I2cError::ArbitrationLoss => ErrorKind::ArbitrationLoss,
        I2cError::Bus => ErrorKind::Bus,
        I2cError::Overrun => ErrorKind::Overrun,
        _ => ErrorKind::Other,
    }
}
