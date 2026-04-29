//! ESB configuration types.

use crate::error::Error;

/// Radio ramp-up time in microseconds (normal mode).
pub const RAMP_UP_US: u16 = 140;
/// Radio ramp-up time in microseconds (fast ramp-up mode).
pub const RAMP_UP_FAST_US: u16 = 40;

/// ESB protocol bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Bitrate {
    /// 1 Mbps (longer range, lower current).
    Mbps1,
    /// 2 Mbps (shorter range, higher throughput) — default for ESB.
    Mbps2,
}

impl Default for Bitrate {
    fn default() -> Self {
        Self::Mbps2
    }
}

/// ESB RF channel (0–100, maps to 2400–2500 MHz).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Channel(pub u8);

impl Channel {
    /// Minimum channel value.
    pub const MIN: u8 = 0;
    /// Maximum channel value.
    pub const MAX: u8 = 100;
}

impl Default for Channel {
    fn default() -> Self {
        Self(2)
    }
}

/// CRC configuration.
///
/// The nRF RADIO CRCPOLY register is 16-bit. For the standard ESB 16-bit CRC,
/// the full polynomial is x^16 + x^12 + x^5 + 1 (0x11021), but only the lower
/// 16 bits (0x1021) are written to the register — the MSB is implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct CrcConfig {
    /// CRC length in bytes: 0 (off), 1, or 2.
    pub length: u8,
    /// Initial CRC value.
    pub init: u16,
    /// Polynomial (lower 16 bits, MSB is implicit in hardware).
    pub poly: u16,
}

impl Default for CrcConfig {
    fn default() -> Self {
        Self {
            length: 2,
            init: 0xFFFF,
            poly: 0x1021,
        }
    }
}

/// Retransmission configuration (PTX only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RetransmitConfig {
    /// Maximum retransmit attempts (0–15).
    pub count: u8,
    /// Delay in microseconds between retransmits.
    ///
    /// Must be > `ack_timeout` + 62 µs. Typical: 500 µs.
    pub delay_us: u16,
}

impl Default for RetransmitConfig {
    fn default() -> Self {
        Self {
            count: 3,
            delay_us: 500,
        }
    }
}

/// Top-level ESB configuration.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct EsbConfig {
    /// Bitrate (1 or 2 Mbps).
    pub bitrate: Bitrate,
    /// RF channel (0–100).
    pub channel: Channel,
    /// CRC configuration.
    pub crc: CrcConfig,
    /// Retransmit configuration (PTX only).
    pub retransmit: RetransmitConfig,
    /// ACK timeout in microseconds (PTX waits this long for ACK).
    ///
    /// Must be >= 44 µs. Default: 120 µs.
    pub ack_timeout_us: u16,
    /// Payload length (1–252 bytes).
    pub payload_length: u8,
}

impl Default for EsbConfig {
    fn default() -> Self {
        Self {
            bitrate: Bitrate::default(),
            channel: Channel::default(),
            crc: CrcConfig::default(),
            retransmit: RetransmitConfig::default(),
            ack_timeout_us: 120,
            payload_length: 32,
        }
    }
}

impl EsbConfig {
    /// Validate configuration against Nordic ESB constraints.
    ///
    /// Returns `Ok(())` if valid, `Err(Error::InvalidParam)` otherwise.
    pub fn validate(&self) -> Result<(), Error> {
        // Payload length
        if self.payload_length > 252 || self.payload_length == 0 {
            return Err(Error::InvalidParam);
        }

        // ACK timeout minimum
        if self.ack_timeout_us < 44 {
            return Err(Error::InvalidParam);
        }

        // Retransmit delay must be > ack_timeout + 62 µs
        let min_retransmit_delay = self.ack_timeout_us.saturating_add(62);
        if self.retransmit.delay_us <= min_retransmit_delay {
            return Err(Error::InvalidParam);
        }

        // Channel range
        if self.channel.0 > Channel::MAX {
            return Err(Error::InvalidParam);
        }

        // Retransmit count range
        if self.retransmit.count > 15 {
            return Err(Error::InvalidParam);
        }

        // CRC length: 0, 1, or 2
        if self.crc.length > 2 {
            return Err(Error::InvalidParam);
        }

        Ok(())
    }
}
