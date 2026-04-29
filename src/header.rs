//! ESB packet header layout for DMA.
//!
//! The hardware DMA header is 2 bytes (length + PID/NO_ACK).
//! The software header adds 2 bytes (RSSI + pipe) before the DMA header,
//! giving a 4-byte struct that maps to the on-air packet format.

/// ESB on-air packet header as stored in the DMA buffer.
///
/// Layout (do not reorder — hardware-dependent):
/// ```text
/// byte 0: rssi       [SW] — filled after RX, not transmitted
/// byte 1: pipe       [SW] — pipe number, not transmitted
/// byte 2: length     [HW] — RADIO PCNF0.LFLEN field
/// byte 3: pid_no_ack [HW] — RADIO PCNF0.S1LEN field (PID[1:0] | NO_ACK)
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
    /// PID (bits 7:6) and NO_ACK flag (bit 0) (hardware DMA field).
    pub pid_no_ack: u8,
}

impl EsbHeader {
    /// Offset from struct start to the hardware DMA fields (length, pid_no_ack).
    pub const DMA_OFFSET: usize = 2;

    /// Maximum ESB payload length.
    pub const MAX_PAYLOAD: u8 = 252;

    /// Extract PID (2-bit packet identifier).
    pub fn pid(&self) -> u8 {
        self.pid_no_ack >> 6
    }

    /// Set PID value (only lower 2 bits used).
    pub fn set_pid(&mut self, pid: u8) {
        self.pid_no_ack = (self.pid_no_ack & !0xC0) | ((pid & 0x03) << 6);
    }

    /// Check if NO_ACK flag is set.
    pub fn no_ack(&self) -> bool {
        self.pid_no_ack & 0x01 != 0
    }

    /// Set NO_ACK flag.
    pub fn set_no_ack(&mut self, no_ack: bool) {
        if no_ack {
            self.pid_no_ack |= 0x01;
        } else {
            self.pid_no_ack &= !0x01;
        }
    }
}
