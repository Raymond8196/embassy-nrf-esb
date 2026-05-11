//! ESB packet header layout for DMA.
//!
//! The hardware DMA header is 2 bytes (length + PID/NO_ACK).
//! The software header adds 2 bytes (RSSI + pipe) before the DMA header,
//! giving a 4-byte struct.
//!
//! # On-air S1 field layout (3 bits)
//!
//! The nRF RADIO S1 field is 3 bits wide (PCNF0.S1LEN=3). The lower 3 bits
//! of `pid_no_ack` map to the S1 field:
//! - Bit 0: NO_ACK flag (1 = transmitter does not want ACK)
//! - Bits 2:1: PID (2-bit packet identifier for duplicate detection)

/// ESB on-air packet header as stored in the DMA buffer.
///
/// Layout (do not reorder — hardware-dependent):
/// ```text
/// byte 0: rssi       [SW] — filled after RX, not transmitted
/// byte 1: pipe       [SW] — pipe number, not transmitted
/// byte 2: length     [HW] — RADIO PCNF0.LFLEN field
/// byte 3: pid_no_ack [HW] — RADIO PCNF0.S1LEN field (3 bits used)
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct EsbHeader {
    /// RSSI value (software-only, filled after receive).
    pub rssi: u8,
    /// Pipe number (software-only, identifies which pipe received).
    pub pipe: u8,
    /// Payload length (hardware DMA field).
    pub length: u8,
    /// PID (bits 2:1) and NO_ACK flag (bit 0). Upper 5 bits unused on air.
    pub pid_no_ack: u8,
}

impl EsbHeader {
    /// Offset from struct start to the hardware DMA fields (length, pid_no_ack).
    pub const DMA_OFFSET: usize = 2;

    /// Offset from struct start to the payload data (after full 4-byte header).
    pub const PAYLOAD_OFFSET: usize = 4;

    /// Maximum ESB payload length.
    pub const MAX_PAYLOAD: u8 = 252;

    /// Extract PID (2-bit packet identifier, bits 2:1).
    pub fn pid(&self) -> u8 {
        (self.pid_no_ack >> 1) & 0x03
    }

    /// Set PID value (only lower 2 bits used, stored in bits 2:1).
    pub fn set_pid(&mut self, pid: u8) {
        self.pid_no_ack = (self.pid_no_ack & !0x06) | ((pid & 0x03) << 1);
    }

    /// Check if NO_ACK flag is set (bit 0 = 1 means no ACK requested).
    pub fn no_ack(&self) -> bool {
        self.pid_no_ack & 0x01 != 0
    }

    /// Set NO_ACK flag (bit 0).
    pub fn set_no_ack(&mut self, no_ack: bool) {
        if no_ack {
            self.pid_no_ack |= 0x01;
        } else {
            self.pid_no_ack &= !0x01;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_header_is_zeroed() {
        let h = EsbHeader::default();
        assert_eq!(h.rssi, 0);
        assert_eq!(h.pipe, 0);
        assert_eq!(h.length, 0);
        assert_eq!(h.pid_no_ack, 0);
    }

    #[test]
    fn pid_roundtrip() {
        let mut h = EsbHeader::default();
        for pid in 0..4u8 {
            h.set_pid(pid);
            assert_eq!(h.pid(), pid, "pid roundtrip failed for {}", pid);
        }
    }

    #[test]
    fn pid_wraps_to_2_bits() {
        let mut h = EsbHeader::default();
        h.set_pid(0xFF);
        assert_eq!(h.pid(), 3);
    }

    #[test]
    fn no_ack_roundtrip() {
        let mut h = EsbHeader::default();
        assert!(!h.no_ack());
        h.set_no_ack(true);
        assert!(h.no_ack());
        h.set_no_ack(false);
        assert!(!h.no_ack());
    }

    #[test]
    fn pid_and_no_ack_independent() {
        let mut h = EsbHeader::default();
        h.set_pid(2);
        h.set_no_ack(true);
        assert_eq!(h.pid(), 2);
        assert!(h.no_ack());

        h.set_no_ack(false);
        assert_eq!(h.pid(), 2);
        assert!(!h.no_ack());

        h.set_pid(0);
        assert_eq!(h.pid(), 0);
        assert!(!h.no_ack());
    }

    #[test]
    fn s1_field_bit_layout() {
        let mut h = EsbHeader::default();
        h.set_pid(0b11);
        h.set_no_ack(true);
        assert_eq!(h.pid_no_ack & 0x07, 0b111);

        h.set_pid(0b10);
        h.set_no_ack(false);
        assert_eq!(h.pid_no_ack & 0x07, 0b100);
    }

    #[test]
    fn consts() {
        assert_eq!(EsbHeader::DMA_OFFSET, 2);
        assert_eq!(EsbHeader::PAYLOAD_OFFSET, 4);
        assert_eq!(EsbHeader::MAX_PAYLOAD, 252);
    }

    #[test]
    fn struct_size_and_alignment() {
        assert_eq!(core::mem::size_of::<EsbHeader>(), 4);
    }
}
