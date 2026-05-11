//! ESB address configuration.
//!
//! ESB uses a 5-byte address per pipe: 4-byte BASE + 1-byte PREFIX.
//! Pipe 0 has its own BASE (base0), Pipes 1–7 share base1 but have unique prefixes.
//!
//! Addresses are stored in on-air byte order. The driver converts to RADIO register
//! format (bit-reversed) internally before writing to BASE0/BASE1/PREFIX registers.
//! This conversion is required for nRF24L01+ on-air compatibility.

/// Maximum number of ESB pipes.
pub const MAX_PIPES: usize = 8;

/// ESB address configuration.
///
/// Pipe 0 uses `base0` + `prefix[0]`. Pipes 1–7 share `base1` + `prefix[1..8]`.
///
/// # Example
///
/// ```
/// use embassy_nrf_esb::addresses::EsbAddresses;
///
/// let addr = EsbAddresses::new(
///     [0xE7, 0xE7, 0xE7, 0xE7], // base0
///     [0xC2, 0xC2, 0xC2, 0xC2], // base1
///     [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
///     8,
/// );
/// assert!(addr.is_ok());
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct EsbAddresses {
    base0: [u8; 4],
    base1: [u8; 4],
    prefix: [u8; MAX_PIPES],
    pipe_count: u8,
}


impl EsbAddresses {
    /// Create a new address configuration.
    ///
    /// Returns an error if `pipe_count` is not in 1–8.
    pub fn new(
        base0: [u8; 4],
        base1: [u8; 4],
        prefix: [u8; MAX_PIPES],
        pipe_count: u8,
    ) -> Result<Self, AddressError> {
        if pipe_count == 0 || pipe_count > MAX_PIPES as u8 {
            return Err(AddressError::InvalidPipeCount(pipe_count));
        }
        Ok(Self {
            base0,
            base1,
            prefix,
            pipe_count,
        })
    }

    /// Get the number of enabled pipes.
    pub fn pipe_count(&self) -> u8 {
        self.pipe_count
    }

    /// Get the enabled pipes as a bitmask (bit 0 = pipe 0, etc.).
    pub fn enabled_mask(&self) -> u8 {
        if self.pipe_count == MAX_PIPES as u8 {
            0xFF
        } else {
            (1u8 << self.pipe_count) - 1
        }
    }

    /// Get the prefix byte for a given pipe.
    ///
    /// Returns an error if pipe >= `pipe_count`.
    pub fn prefix_for_pipe(&self, pipe: u8) -> Result<u8, AddressError> {
        if pipe >= self.pipe_count {
            return Err(AddressError::InvalidPipe(pipe));
        }
        Ok(self.prefix[pipe as usize])
    }

    /// Convert base address to RADIO register format (bit-reversed u32).
    ///
    /// The nRF RADIO peripheral uses bit-reversed address order compared
    /// to on-air transmission. This conversion is required for nRF24L01+
    /// compatibility.
    pub(crate) fn base0_reg(&self) -> u32 {
        reverse_bits(u32::from_le_bytes(self.base0))
    }

    /// Convert base1 address to RADIO register format.
    pub(crate) fn base1_reg(&self) -> u32 {
        reverse_bits(u32::from_le_bytes(self.base1))
    }

    /// Convert prefix bytes to RADIO PREFIX0 register value.
    ///
    /// PREFIX0 holds prefixes for pipes 0–3. Each byte is bit-reversed
    /// and packed into a u32.
    pub(crate) fn prefix0_reg(&self) -> u32 {
        bytewise_bit_swap(u32::from_le_bytes([
            self.prefix[0],
            self.prefix[1],
            self.prefix[2],
            self.prefix[3],
        ]))
    }

    /// Convert prefix bytes to RADIO PREFIX1 register value.
    ///
    /// PREFIX1 holds prefixes for pipes 4–7.
    pub(crate) fn prefix1_reg(&self) -> u32 {
        bytewise_bit_swap(u32::from_le_bytes([
            self.prefix[4],
            self.prefix[5],
            self.prefix[6],
            self.prefix[7],
        ]))
    }
}

/// Reverse all bits in a u32 (address conversion for RADIO registers).
#[inline]
fn reverse_bits(value: u32) -> u32 {
    value.reverse_bits()
}

/// Reverse bits within each byte of a u32, keeping byte order.
/// Used for prefix register conversion.
#[inline]
fn bytewise_bit_swap(value: u32) -> u32 {
    value.reverse_bits().swap_bytes()
}

/// Address configuration errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum AddressError {
    /// Pipe count must be 1–8.
    InvalidPipeCount(u8),
    /// Pipe index out of range (must be < pipe_count).
    InvalidPipe(u8),
}

/// Default ESB addresses matching Nordic SDK defaults.
impl Default for EsbAddresses {
    fn default() -> Self {
        Self {
            base0: [0xE7, 0xE7, 0xE7, 0xE7],
            base1: [0xC2, 0xC2, 0xC2, 0xC2],
            prefix: [0xE7, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8],
            pipe_count: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_valid_pipe_counts() {
        for count in 1..=8u8 {
            assert!(EsbAddresses::new(
                [0; 4], [0; 4], [0; 8], count
            ).is_ok());
        }
    }

    #[test]
    fn new_rejects_zero_pipes() {
        let r = EsbAddresses::new([0; 4], [0; 4], [0; 8], 0);
        assert_eq!(r.unwrap_err(), AddressError::InvalidPipeCount(0));
    }

    #[test]
    fn new_rejects_too_many_pipes() {
        let r = EsbAddresses::new([0; 4], [0; 4], [0; 8], 9);
        assert_eq!(r.unwrap_err(), AddressError::InvalidPipeCount(9));
    }

    #[test]
    fn enabled_mask_all_pipes() {
        let addr = EsbAddresses::default();
        assert_eq!(addr.enabled_mask(), 0xFF);
    }

    #[test]
    fn enabled_mask_partial() {
        let addr = EsbAddresses::new(
            [0; 4], [0; 4], [0; 8], 3
        ).unwrap();
        assert_eq!(addr.enabled_mask(), 0b111);
    }

    #[test]
    fn enabled_mask_single_pipe() {
        let addr = EsbAddresses::new(
            [0; 4], [0; 4], [0; 8], 1
        ).unwrap();
        assert_eq!(addr.enabled_mask(), 0b1);
    }

    #[test]
    fn prefix_for_valid_pipe() {
        let addr = EsbAddresses::default();
        assert_eq!(addr.prefix_for_pipe(0).unwrap(), 0xE7);
        assert_eq!(addr.prefix_for_pipe(1).unwrap(), 0xC2);
        assert_eq!(addr.prefix_for_pipe(7).unwrap(), 0xC8);
    }

    #[test]
    fn prefix_for_invalid_pipe() {
        let addr = EsbAddresses::new(
            [0; 4], [0; 4], [0; 8], 3
        ).unwrap();
        assert_eq!(
            addr.prefix_for_pipe(3).unwrap_err(),
            AddressError::InvalidPipe(3)
        );
    }

    #[test]
    fn default_matches_nordic_sdk() {
        let addr = EsbAddresses::default();
        assert_eq!(addr.pipe_count(), 8);
        assert_eq!(addr.prefix_for_pipe(0).unwrap(), 0xE7);
        assert_eq!(addr.prefix_for_pipe(1).unwrap(), 0xC2);
    }

    #[test]
    fn reverse_bits_known_values() {
        assert_eq!(reverse_bits(0x00000000), 0x00000000);
        assert_eq!(reverse_bits(0xFFFFFFFF), 0xFFFFFFFF);
        assert_eq!(reverse_bits(0x80000000), 0x00000001);
        assert_eq!(reverse_bits(0x00000001), 0x80000000);
    }

    #[test]
    fn bytewise_bit_swap_identity() {
        assert_eq!(bytewise_bit_swap(0x00000000), 0x00000000);
        assert_eq!(bytewise_bit_swap(0xFFFFFFFF), 0xFFFFFFFF);
    }

    #[test]
    fn bytewise_bit_swap_reverses_within_bytes() {
        let val = u32::from_le_bytes([0x80, 0x00, 0x00, 0x00]);
        let swapped = bytewise_bit_swap(val);
        let bytes = swapped.to_le_bytes();
        assert_eq!(bytes[0], 0x01);
    }

    #[test]
    fn base_reg_is_bit_reversed() {
        let addr = EsbAddresses::new(
            [0x01, 0x02, 0x03, 0x04],
            [0; 4],
            [0; 8],
            1,
        ).unwrap();
        let reg = addr.base0_reg();
        let expected = reverse_bits(u32::from_le_bytes([0x01, 0x02, 0x03, 0x04]));
        assert_eq!(reg, expected);
    }
}
