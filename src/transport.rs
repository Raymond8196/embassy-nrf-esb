//! Small transport framing helpers for higher-level split protocols.
//!
//! The ESB core transmits arbitrary payload bytes. RMK integration should keep
//! carrying RMK's existing serialized `SplitMessage` bytes and add only a thin
//! transport header for routing and duplicate suppression.

use crate::error::Error;

/// Current ESB transport framing version.
pub const TRANSPORT_VERSION: u8 = 1;

/// Header size in bytes.
pub const TRANSPORT_HEADER_LEN: usize = 5;

/// Return the ESB payload length required to carry a framed higher-level
/// payload of `payload_len` bytes.
pub const fn required_esb_payload_len(payload_len: usize) -> usize {
    TRANSPORT_HEADER_LEN + payload_len
}

/// Return whether an ESB `payload_length` can carry a framed higher-level
/// payload of `payload_len` bytes.
pub const fn fits_esb_payload(esb_payload_len: u8, payload_len: usize) -> bool {
    required_esb_payload_len(payload_len) <= esb_payload_len as usize
}

/// Header used by higher-level split protocols carried over ESB.
///
/// Layout:
///
/// ```text
/// byte 0: protocol version
/// byte 1: device id
/// byte 2: sequence number
/// byte 3: flags
/// byte 4: payload length
/// byte 5..: serialized higher-level payload
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct TransportHeader {
    /// Framing version. Must match [`TRANSPORT_VERSION`].
    pub version: u8,
    /// Application-level device id. For RMK MVP this maps to the static binding table.
    pub device_id: u8,
    /// Application-level sequence number for duplicate suppression.
    pub sequence: u8,
    /// Transport flags reserved for future use.
    pub flags: u8,
    /// Serialized payload length after the header.
    pub payload_len: u8,
}

impl TransportHeader {
    /// Create a v1 header.
    pub fn new(device_id: u8, sequence: u8, flags: u8, payload_len: usize) -> Result<Self, Error> {
        if payload_len > u8::MAX as usize {
            return Err(Error::InvalidParam);
        }

        Ok(Self {
            version: TRANSPORT_VERSION,
            device_id,
            sequence,
            flags,
            payload_len: payload_len as u8,
        })
    }

    /// Encode the header into the beginning of `out`.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, Error> {
        if self.version != TRANSPORT_VERSION || out.len() < TRANSPORT_HEADER_LEN {
            return Err(Error::InvalidParam);
        }

        out[0] = self.version;
        out[1] = self.device_id;
        out[2] = self.sequence;
        out[3] = self.flags;
        out[4] = self.payload_len;
        Ok(TRANSPORT_HEADER_LEN)
    }

    /// Decode a header from a full frame buffer.
    pub fn decode(frame: &[u8]) -> Result<Self, Error> {
        if frame.len() < TRANSPORT_HEADER_LEN {
            return Err(Error::InvalidParam);
        }

        let header = Self {
            version: frame[0],
            device_id: frame[1],
            sequence: frame[2],
            flags: frame[3],
            payload_len: frame[4],
        };

        if header.version != TRANSPORT_VERSION {
            return Err(Error::InvalidParam);
        }

        let total_len = TRANSPORT_HEADER_LEN + header.payload_len as usize;
        if frame.len() < total_len {
            return Err(Error::InvalidParam);
        }

        Ok(header)
    }
}

/// Encode a complete frame into `out`.
pub fn encode_frame(
    device_id: u8,
    sequence: u8,
    flags: u8,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, Error> {
    let header = TransportHeader::new(device_id, sequence, flags, payload.len())?;
    let total_len = TRANSPORT_HEADER_LEN + payload.len();
    if out.len() < total_len {
        return Err(Error::InvalidParam);
    }

    header.encode_into(out)?;
    out[TRANSPORT_HEADER_LEN..total_len].copy_from_slice(payload);
    Ok(total_len)
}

/// Decode a complete frame into `(header, payload)`.
pub fn decode_frame(frame: &[u8]) -> Result<(TransportHeader, &[u8]), Error> {
    let header = TransportHeader::decode(frame)?;
    let start = TRANSPORT_HEADER_LEN;
    let end = start + header.payload_len as usize;
    Ok((header, &frame[start..end]))
}

/// Per-device sequence tracker for duplicate suppression above ESB PID/CRC.
///
/// ESB PID detects radio-level repeats on a pipe. Keyboard firmware still
/// needs application-level deduplication keyed by `device_id + sequence`.
#[derive(Debug, Clone, Copy)]
pub struct SequenceTracker<const N: usize> {
    valid: [bool; N],
    last: [u8; N],
}

impl<const N: usize> SequenceTracker<N> {
    /// Create an empty tracker.
    pub const fn new() -> Self {
        Self {
            valid: [false; N],
            last: [0; N],
        }
    }

    /// Return `Ok(true)` for a new sequence, `Ok(false)` for a duplicate.
    pub fn accept(&mut self, device_id: u8, sequence: u8) -> Result<bool, Error> {
        let index = device_id as usize;
        if index >= N {
            return Err(Error::InvalidParam);
        }

        if self.valid[index] && self.last[index] == sequence {
            return Ok(false);
        }

        self.valid[index] = true;
        self.last[index] = sequence;
        Ok(true)
    }

    /// Clear one device entry.
    pub fn reset_device(&mut self, device_id: u8) -> Result<(), Error> {
        let index = device_id as usize;
        if index >= N {
            return Err(Error::InvalidParam);
        }

        self.valid[index] = false;
        self.last[index] = 0;
        Ok(())
    }
}

impl<const N: usize> Default for SequenceTracker<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SequenceTracker, TRANSPORT_HEADER_LEN, TRANSPORT_VERSION, TransportHeader, decode_frame,
        encode_frame, fits_esb_payload, required_esb_payload_len,
    };
    use crate::error::Error;

    #[test]
    fn frame_round_trips_header_and_payload() {
        let payload = [1, 2, 3, 4];
        let mut frame = [0u8; 16];

        let len = encode_frame(2, 7, 0x80, &payload, &mut frame).unwrap();
        assert_eq!(len, TRANSPORT_HEADER_LEN + payload.len());

        let (header, decoded_payload) = decode_frame(&frame[..len]).unwrap();
        assert_eq!(
            header,
            TransportHeader {
                version: TRANSPORT_VERSION,
                device_id: 2,
                sequence: 7,
                flags: 0x80,
                payload_len: 4,
            }
        );
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn rejects_truncated_or_wrong_version_frames() {
        assert_eq!(decode_frame(&[]), Err(Error::InvalidParam));
        assert_eq!(
            decode_frame(&[TRANSPORT_VERSION, 1, 2, 3, 4]),
            Err(Error::InvalidParam)
        );
        assert_eq!(decode_frame(&[0xff, 1, 2, 3, 0]), Err(Error::InvalidParam));
    }

    #[test]
    fn encode_frame_requires_output_capacity() {
        let mut frame = [0u8; TRANSPORT_HEADER_LEN];
        assert_eq!(
            encode_frame(0, 0, 0, &[1], &mut frame),
            Err(Error::InvalidParam)
        );
    }

    #[test]
    fn required_payload_length_includes_transport_header() {
        assert_eq!(required_esb_payload_len(0), TRANSPORT_HEADER_LEN);
        assert_eq!(required_esb_payload_len(32), TRANSPORT_HEADER_LEN + 32);
        assert!(fits_esb_payload(37, 32));
        assert!(!fits_esb_payload(36, 32));
    }

    #[test]
    fn sequence_tracker_accepts_new_and_rejects_duplicates_per_device() {
        let mut tracker = SequenceTracker::<2>::new();

        assert_eq!(tracker.accept(0, 1), Ok(true));
        assert_eq!(tracker.accept(0, 1), Ok(false));
        assert_eq!(tracker.accept(0, 2), Ok(true));
        assert_eq!(tracker.accept(1, 1), Ok(true));
        assert_eq!(tracker.accept(2, 1), Err(Error::InvalidParam));

        tracker.reset_device(0).unwrap();
        assert_eq!(tracker.accept(0, 2), Ok(true));
    }
}
