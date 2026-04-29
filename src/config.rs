//! ESB configuration types.

/// ESB protocol bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Bitrate {
    /// 1 Mbps (longer range, lower current)
    Mbps1,
    /// 2 Mbps (shorter range, higher throughput)
    Mbps2,
}

impl Default for Bitrate {
    fn default() -> Self {
        Self::Mbps2
    }
}

/// ESB protocol mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Mode {
    /// Primary Transmitter — initiates communication.
    Ptx,
    /// Primary Receiver — listens for incoming packets.
    Prx,
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
    /// Delay in microseconds (250, 500, 1000, 1500, … 6000).
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
    /// RF channel.
    pub channel: Channel,
    /// CRC configuration.
    pub crc: CrcConfig,
    /// Retransmit configuration (PTX only).
    pub retransmit: RetransmitConfig,
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
            payload_length: 32,
        }
    }
}
