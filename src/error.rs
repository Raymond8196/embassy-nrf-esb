//! ESB error types.

/// ESB operation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// TX buffer full — no space to queue another packet.
    TxFull,
    /// RX buffer empty — no packet available.
    RxEmpty,
    /// Invalid parameter (e.g. payload length > 252).
    InvalidParam,
    /// Maximum retransmits reached (PTX).
    MaxRetransmit,
    /// Radio not initialized or in wrong state.
    NotReady,
    /// Address configuration error.
    InvalidAddress,
    /// Packet pool exhausted — no free buffers for DMA.
    OutOfMemory,
    /// Cannot suspend — state machine is mid-transaction.
    Busy,
}

const _: () = {
    const fn _assert_traits()
    where
        Error: Copy + Eq,
    {
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_variants_are_distinct() {
        let variants = [
            Error::TxFull,
            Error::RxEmpty,
            Error::InvalidParam,
            Error::MaxRetransmit,
            Error::NotReady,
            Error::InvalidAddress,
            Error::OutOfMemory,
            Error::Busy,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn error_is_copy() {
        let e = Error::TxFull;
        let e2 = e;
        assert_eq!(e, e2);
    }
}
