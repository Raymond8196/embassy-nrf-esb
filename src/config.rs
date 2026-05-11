//! ESB configuration types.

use crate::error::Error;

/// TX output power.
///
/// Maps to nRF RADIO TXPOWER register values (PS §6.17.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TxPower {
    /// -40 dBm.
    Neg40dBm,
    /// -20 dBm.
    Neg20dBm,
    /// -16 dBm.
    Neg16dBm,
    /// -12 dBm.
    Neg12dBm,
    /// -8 dBm.
    Neg8dBm,
    /// -4 dBm.
    Neg4dBm,
    /// 0 dBm (default).
    #[default]
    ZeroDbm,
    /// +2 dBm.
    Pos2dBm,
    /// +3 dBm.
    Pos3dBm,
    /// +4 dBm.
    Pos4dBm,
    /// +5 dBm.
    Pos5dBm,
    /// +6 dBm.
    Pos6dBm,
    /// +7 dBm.
    Pos7dBm,
    /// +8 dBm.
    Pos8dBm,
}

impl TxPower {
    /// Convert to PAC TXPOWER register value.
    pub(crate) fn to_pac(self) -> crate::pac::radio::vals::Txpower {
        use crate::pac::radio::vals::Txpower;
        match self {
            Self::Neg40dBm => Txpower::NEG40_DBM,
            Self::Neg20dBm => Txpower::NEG20_DBM,
            Self::Neg16dBm => Txpower::NEG16_DBM,
            Self::Neg12dBm => Txpower::NEG12_DBM,
            Self::Neg8dBm => Txpower::NEG8_DBM,
            Self::Neg4dBm => Txpower::NEG4_DBM,
            Self::ZeroDbm => Txpower::_0_DBM,
            Self::Pos2dBm => Txpower::POS2_DBM,
            Self::Pos3dBm => Txpower::POS3_DBM,
            Self::Pos4dBm => Txpower::POS4_DBM,
            Self::Pos5dBm => Txpower::POS5_DBM,
            Self::Pos6dBm => Txpower::POS6_DBM,
            Self::Pos7dBm => Txpower::POS7_DBM,
            Self::Pos8dBm => Txpower::POS8_DBM,
        }
    }
}

/// Radio ramp-up time in microseconds (normal mode).
pub const RAMP_UP_US: u16 = 140;
/// Radio ramp-up time in microseconds (fast ramp-up mode).
pub const RAMP_UP_FAST_US: u16 = 40;

/// ESB protocol bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Bitrate {
    /// 1 Mbps (longer range, lower current).
    Mbps1,
    /// 2 Mbps (shorter range, higher throughput) — default for ESB.
    #[default]
    Mbps2,
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
    /// TX output power.
    pub tx_power: TxPower,
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
            tx_power: TxPower::default(),
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
        // AND > RAMP_UP_TIME (radio needs time to re-enable)
        let min_retransmit_delay = self.ack_timeout_us.saturating_add(62);
        if self.retransmit.delay_us <= min_retransmit_delay
            || self.retransmit.delay_us <= RAMP_UP_US
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        assert!(EsbConfig::default().validate().is_ok());
    }

    #[test]
    fn payload_zero_rejected() {
        let mut c = EsbConfig::default();
        c.payload_length = 0;
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
    }

    #[test]
    fn payload_253_rejected() {
        let mut c = EsbConfig::default();
        c.payload_length = 253;
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
    }

    #[test]
    fn payload_252_accepted() {
        let mut c = EsbConfig::default();
        c.payload_length = 252;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ack_timeout_below_44_rejected() {
        let mut c = EsbConfig::default();
        c.ack_timeout_us = 43;
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
    }

    #[test]
    fn ack_timeout_44_accepted() {
        let mut c = EsbConfig::default();
        c.ack_timeout_us = 44;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn retransmit_delay_must_exceed_ack_plus_62() {
        let mut c = EsbConfig::default();
        c.ack_timeout_us = 120;
        c.retransmit.delay_us = 182; // == ack_timeout + 62, should fail (<= check)
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
        c.retransmit.delay_us = 183;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn retransmit_delay_must_exceed_ramp_up() {
        let mut c = EsbConfig::default();
        c.ack_timeout_us = 44;
        c.retransmit.delay_us = RAMP_UP_US; // == RAMP_UP, should fail
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
        c.retransmit.delay_us = RAMP_UP_US + 1;
        // But also must exceed ack_timeout + 62
        c.retransmit.delay_us = c.retransmit.delay_us.max(c.ack_timeout_us + 63);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn channel_over_100_rejected() {
        let mut c = EsbConfig::default();
        c.channel = Channel(101);
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
    }

    #[test]
    fn channel_100_accepted() {
        let mut c = EsbConfig::default();
        c.channel = Channel(100);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn retransmit_count_over_15_rejected() {
        let mut c = EsbConfig::default();
        c.retransmit.count = 16;
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
    }

    #[test]
    fn crc_length_3_rejected() {
        let mut c = EsbConfig::default();
        c.crc.length = 3;
        assert_eq!(c.validate().unwrap_err(), Error::InvalidParam);
    }

    #[test]
    fn crc_length_0_accepted() {
        let mut c = EsbConfig::default();
        c.crc.length = 0;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn default_channel_is_2() {
        assert_eq!(Channel::default().0, 2);
    }

    #[test]
    fn default_bitrate_is_2mbps() {
        assert_eq!(Bitrate::default(), Bitrate::Mbps2);
    }

    #[test]
    fn default_crc_is_16bit() {
        let crc = CrcConfig::default();
        assert_eq!(crc.length, 2);
        assert_eq!(crc.init, 0xFFFF);
        assert_eq!(crc.poly, 0x1021);
    }

    #[test]
    fn default_retransmit() {
        let rt = RetransmitConfig::default();
        assert_eq!(rt.count, 3);
        assert_eq!(rt.delay_us, 500);
    }
}
