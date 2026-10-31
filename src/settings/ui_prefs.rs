//! Small UI preference types: pill size, waveform sensitivity, and the
//! live-preview control state.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillSize {
    Compact,
    Standard,
}

impl PillSize {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
        }
    }

    pub(super) fn resolve(saved: Option<&str>, override_value: Option<&str>) -> Self {
        match override_value
            .or(saved)
            .unwrap_or("compact")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "standard" => Self::Standard,
            _ => Self::Compact,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveformSensitivity {
    Low,
    Balanced,
    High,
}

impl WaveformSensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Balanced => "balanced",
            Self::High => "high",
        }
    }

    pub(super) fn resolve(saved: Option<&str>, override_value: Option<&str>) -> Self {
        match override_value
            .or(saved)
            .unwrap_or("balanced")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "low" => Self::Low,
            "high" => Self::High,
            _ => Self::Balanced,
        }
    }
}

/// Derived UI state; the saved streaming preference remains unchanged when
/// always-on capture temporarily makes preview unavailable.
#[derive(Debug, PartialEq, Eq)]
pub struct LivePreviewControl {
    pub enabled: bool,
    pub editable: bool,
    pub hint: &'static str,
}

impl LivePreviewControl {
    pub(super) fn resolve(saved: Option<bool>, override_value: Option<&str>, preroll_ms: u32) -> Self {
        let enabled = resolve_enabled_flag(saved, override_value, true);
        if preroll_ms > 0 {
            Self {
                enabled: false,
                editable: false,
                hint: "Turn off Always-on mic to use live preview.",
            }
        } else if override_value.is_some() {
            Self {
                enabled,
                editable: false,
                hint: "Controlled by DICTATE_STREAMING_ASR.",
            }
        } else {
            Self {
                enabled,
                editable: true,
                hint: "Draft phrases appear as you pause.",
            }
        }
    }
}
