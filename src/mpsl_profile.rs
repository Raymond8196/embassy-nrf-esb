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
    /// Same narrow pipe-1 diagnostic with PRX-hint phase-locked PTX scheduling.
    DiagnosticPipe1ScheduledGate,
    DiagnosticPipe2ScheduledGate,
    DiagnosticPipe5ScheduledGate,
    /// Same narrow pipe-1 diagnostic with an 8 ms PRX receive window.
    DiagnosticPipe1Prx8ms,
    /// Same narrow pipe-1 diagnostic with a 12 ms PRX receive window.
    DiagnosticPipe1Prx12ms,
    /// Same narrow pipe-1 diagnostic with a 20 ms PRX receive window.
    DiagnosticPipe1Prx20ms,
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
    /// Optional PTX scheduling policy derived from PRX ACK hints.
    pub schedule_gate: PtxScheduleGateConfig,
}

/// PTX scheduling mode derived from PRX schedule hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PtxScheduleMode {
    /// Always request the next PTX timeslot as soon as MPSL can provide it.
    Disabled,
    /// Diagnostic-only policy that skips a fixed number of PTX slots after a hint.
    FixedSkipAfterHint,
    /// Lock subsequent PTX timeslots to the hinted PRX period using MPSL NORMAL requests.
    PhaseLocked,
}

/// PTX pacing policy derived from PRX schedule hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PtxScheduleGateConfig {
    /// Scheduling strategy.
    pub mode: PtxScheduleMode,
    /// Number of PTX timeslots to skip after receiving a valid hint in fixed-skip mode.
    pub skip_after_hint_slots: u8,
    /// Consecutive TX misses tolerated before phase-locked mode falls back to scanning.
    pub lock_miss_limit: u8,
    /// Extra offset from the estimated PRX window start before transmitting.
    pub lock_tx_offset_us: u32,
}

impl PtxScheduleGateConfig {
    pub const fn disabled() -> Self {
        Self {
            mode: PtxScheduleMode::Disabled,
            skip_after_hint_slots: 0,
            lock_miss_limit: 0,
            lock_tx_offset_us: 0,
        }
    }

    pub const fn conservative_pipe1() -> Self {
        Self::phase_locked_pipe1()
    }

    pub const fn fixed_skip_pipe1() -> Self {
        Self {
            mode: PtxScheduleMode::FixedSkipAfterHint,
            skip_after_hint_slots: 2,
            lock_miss_limit: 0,
            lock_tx_offset_us: 0,
        }
    }

    pub const fn phase_locked_pipe1() -> Self {
        Self {
            mode: PtxScheduleMode::PhaseLocked,
            skip_after_hint_slots: 0,
            lock_miss_limit: 4,
            lock_tx_offset_us: 250,
        }
    }

    pub fn validate(self) -> Result<(), Error> {
        match self.mode {
            PtxScheduleMode::Disabled => {}
            PtxScheduleMode::FixedSkipAfterHint => {
                if self.skip_after_hint_slots == 0 {
                    return Err(Error::InvalidParam);
                }
            }
            PtxScheduleMode::PhaseLocked => {
                if self.lock_miss_limit == 0 {
                    return Err(Error::InvalidParam);
                }
            }
        }

        Ok(())
    }
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
            schedule_gate: PtxScheduleGateConfig::disabled(),
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
        self.schedule_gate.validate()?;
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1ScheduledGate => Self {
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
                    schedule_gate: PtxScheduleGateConfig::phase_locked_pipe1(),
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe2ScheduledGate => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x06,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 1500,
                    in_slot_match_us: 1300,
                    pipe_mask: 0x06,
                    report_every: 2,
                    ack_timeout_us: 400,
                    max_retries: 0,
                    request,
                    schedule_gate: PtxScheduleGateConfig::phase_locked_pipe1(),
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe5ScheduledGate => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 5000,
                    in_slot_match_us: 4500,
                    report_every: 20,
                    enabled_pipes: 0x3E,
                    request,
                    recovery,
                },
                ptx: PtxPollConfig {
                    slot_length_us: 2500,
                    in_slot_match_us: 2300,
                    pipe_mask: 0x3E,
                    report_every: 5,
                    ack_timeout_us: 400,
                    max_retries: 1,
                    request,
                    schedule_gate: PtxScheduleGateConfig::phase_locked_pipe1(),
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1Prx8ms => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 8000,
                    in_slot_match_us: 7600,
                    report_every: 12,
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1Prx12ms => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 12_000,
                    in_slot_match_us: 11_500,
                    report_every: 8,
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
                },
                ble_hint: BleCoexistenceHint::DiagnosticOnly,
            },
            CoexistenceProfile::DiagnosticPipe1Prx20ms => Self {
                prx: PrxSlotConfig {
                    slot_length_us: 20_000,
                    in_slot_match_us: 19_000,
                    report_every: 5,
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
                    schedule_gate: PtxScheduleGateConfig::disabled(),
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
