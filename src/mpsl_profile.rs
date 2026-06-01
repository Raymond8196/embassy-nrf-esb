//! Named MPSL coexistence profiles and their derived diagnostic configs.

use crate::error::Error;
use crate::mpsl_radio::RadioRecoveryPolicy;

/// These are starting points for hardware runs, not product guarantees. The
/// explicit config structs can be edited after choosing a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CoexistenceProfile {
    /// Narrow pipe-1 diagnostic used to classify ACK misses.
    DiagnosticPipe1,
    /// Same narrow pipe-1 diagnostic with a longer ACK wait window.
    DiagnosticPipe1RelaxedAck,
    /// Same narrow pipe-1 diagnostic with one in-slot retry after ACK timeout.
    DiagnosticPipe1Retry1,
    /// Same narrow pipe-1 diagnostic with a longer PTX slot and ACK window.
    DiagnosticPipe1LongSlot,
    /// Advertising-visible BLE coexistence smoke profile.
    AdvertisingCoexistence,
    /// Active BLE connection profile assuming relaxed connection parameters.
    ConnectedRelaxedBle,
    /// Low-density keyboard traffic starting point for RMK-style smoke tests.
    RmkKeyboardLowLatency,
}

/// BLE-side assumption attached to a coexistence profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BleCoexistenceHint {
    DiagnosticOnly,
    AdvertisingVisible,
    ConnectedRelaxed,
    RmkKeyboardLowLatency,
}

/// Timeslot request policy shared by PRX/PTX configs in a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct TimeslotRequestConfig {
    /// Timeout used for earliest timeslot requests.
    pub timeout_us: u32,
    /// Whether BLOCKED/CANCELLED recovery should retry at high priority.
    pub retry_blocked_at_high_priority: bool,
}

impl TimeslotRequestConfig {
    pub const fn diagnostic_default() -> Self {
        Self {
            timeout_us: 1_000_000,
            retry_blocked_at_high_priority: true,
        }
    }

    pub fn validate(self) -> Result<(), Error> {
        if self.timeout_us == 0 {
            return Err(Error::InvalidParam);
        }

        Ok(())
    }
}

/// Configuration for a long-lived PRX timeslot session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PrxSlotConfig {
    /// Timeslot duration per PRX window.
    pub slot_length_us: u32,
    /// TIMER0 compare value within the slot. Must be less than `slot_length_us`.
    pub in_slot_match_us: u32,
    /// Number of slots per report.
    pub report_every: u32,
    /// Enabled ESB RX pipe mask.
    pub enabled_pipes: u8,
    /// Timeslot request policy.
    pub request: TimeslotRequestConfig,
    /// RADIO recovery policy before handing the peripheral back to MPSL/SDC.
    pub recovery: RadioRecoveryPolicy,
}

impl PrxSlotConfig {
    pub const fn for_profile(profile: CoexistenceProfile) -> Self {
        CoexistenceProfileConfig::for_profile(profile).prx
    }

    pub fn validate(self) -> Result<(), Error> {
        if self.slot_length_us == 0
            || self.in_slot_match_us == 0
            || self.in_slot_match_us >= self.slot_length_us
            || self.report_every == 0
            || self.enabled_pipes == 0
        {
            return Err(Error::InvalidParam);
        }

        self.request.validate()?;
        Ok(())
    }
}

/// Configuration for a long-lived PTX poll session.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PtxPollConfig {
    /// Timeslot duration per poll.
    pub slot_length_us: u32,
    /// TIMER0 compare value within the slot. Must be less than `slot_length_us`.
    pub in_slot_match_us: u32,
    /// Bitmask of pipes to poll.
    pub pipe_mask: u8,
    /// Number of slots per report.
    pub report_every: u32,
    /// ACK wait window after TX completion.
    pub ack_timeout_us: u32,
    /// In-slot retries after ACK timeout. Use 0 to keep misses visible.
    pub max_retries: u8,
    /// Timeslot request policy.
    pub request: TimeslotRequestConfig,
}

impl PtxPollConfig {
    /// Build a poll config from a named coexistence profile.
    pub const fn for_profile(profile: CoexistenceProfile) -> Self {
        CoexistenceProfileConfig::for_profile(profile).ptx
    }

    /// Build a diagnostic poll config with no in-slot retries.
    pub const fn diagnostic(
        slot_length_us: u32,
        in_slot_match_us: u32,
        pipe_mask: u8,
        report_every: u32,
    ) -> Self {
        Self {
            slot_length_us,
            in_slot_match_us,
            pipe_mask,
            report_every,
            ack_timeout_us: 400,
            max_retries: 0,
            request: TimeslotRequestConfig::diagnostic_default(),
        }
    }

    pub const fn pipe_count(self) -> u32 {
        self.pipe_mask.count_ones()
    }

    pub fn validate(self) -> Result<(), Error> {
        if self.slot_length_us == 0
            || self.in_slot_match_us == 0
            || self.in_slot_match_us >= self.slot_length_us
            || self.pipe_mask == 0
            || self.report_every == 0
            || self.ack_timeout_us == 0
        {
            return Err(Error::InvalidParam);
        }

        self.request.validate()?;
        Ok(())
    }
}

/// Single source for the PRX/PTX/MPSL assumptions attached to a named profile.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct CoexistenceProfileConfig {
    pub prx: PrxSlotConfig,
    pub ptx: PtxPollConfig,
    pub ble_hint: BleCoexistenceHint,
}

impl CoexistenceProfileConfig {
    pub const fn for_profile(profile: CoexistenceProfile) -> Self {
        let request = TimeslotRequestConfig::diagnostic_default();
        let recovery = RadioRecoveryPolicy::ForceResetAfterBoundedDisable;

        match profile {
            CoexistenceProfile::DiagnosticPipe1 => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x02,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 1500,
                    in_slot_match_us: 1300,
                    pipe_mask: 0x02,
                    report_every: 1,
                    ack_timeout_us: 400,
                    max_retries: 0,
                    request,
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1RelaxedAck => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x02,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 1500,
                    in_slot_match_us: 1300,
                    pipe_mask: 0x02,
                    report_every: 1,
                    ack_timeout_us: 600,
                    max_retries: 0,
                    request,
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1Retry1 => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x02,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 1500,
                    in_slot_match_us: 1300,
                    pipe_mask: 0x02,
                    report_every: 1,
                    ack_timeout_us: 400,
                    max_retries: 1,
                    request,
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1LongSlot => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x02,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 3000,
                    in_slot_match_us: 2800,
                    pipe_mask: 0x02,
                    report_every: 1,
                    ack_timeout_us: 600,
                    max_retries: 1,
                    request,
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::AdvertisingCoexistence => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 12_000,
                    in_slot_match_us: 11_500,
                    report_every: 50,
                    enabled_pipes: 0x06,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 3000,
                    in_slot_match_us: 2800,
                    pipe_mask: 0x06,
                    report_every: 2,
                    ack_timeout_us: 400,
                    max_retries: 0,
                    request,
                },
                ble_hint: BleCoexistenceHint::AdvertisingVisible,
            },
            CoexistenceProfile::ConnectedRelaxedBle => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x02,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 1500,
                    in_slot_match_us: 1300,
                    pipe_mask: 0x02,
                    report_every: 1,
                    ack_timeout_us: 400,
                    max_retries: 0,
                    request,
                },
                ble_hint: BleCoexistenceHint::ConnectedRelaxed,
            },
            CoexistenceProfile::RmkKeyboardLowLatency => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 3000,
                    in_slot_match_us: 2800,
                    report_every: 4,
                    enabled_pipes: 0x02,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 3000,
                    in_slot_match_us: 2800,
                    pipe_mask: 0x02,
                    report_every: 1,
                    ack_timeout_us: 400,
                    max_retries: 1,
                    request,
                },
                ble_hint: BleCoexistenceHint::RmkKeyboardLowLatency,
            },
        }
    }

    pub fn validate(self) -> Result<(), Error> {
        self.prx.validate()?;
        self.ptx.validate()?;
        Ok(())
    }
}
