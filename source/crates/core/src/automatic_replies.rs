//! Automatic replies (Out-of-Office) pure domain model (OUTLOOK_PARITY.md §5.1
//! M-24, roadmap item 2.11).
//!
//! One [`AutomaticReplies`] value describes what an account answers while its
//! owner is away, shared by item 2.11's two backends: Microsoft 365's Graph
//! `automaticRepliesSetting` (all fields) and a Sieve `vacation` block (the
//! single message plus the schedule; the internal/external split is Graph-only,
//! so [`AutomaticReplies::internal_message`] is the one the Sieve compiler
//! emits). Like `rules.rs`, this module has no I/O and holds only the shape
//! plus pure state logic.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Utc};

/// Where an account's automatic replies are (Graph's
/// `automaticRepliesSetting.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum AutomaticReplyStatus {
    #[default]
    Disabled,
    /// Reply at all times until turned off ("on" in the UI).
    AlwaysEnabled,
    /// Reply between [`AutomaticReplies::start`] and [`AutomaticReplies::end`].
    Scheduled,
}

impl AutomaticReplyStatus {
    /// The Graph `status` value.
    pub fn as_graph_str(self) -> &'static str {
        match self {
            AutomaticReplyStatus::Disabled => "disabled",
            AutomaticReplyStatus::AlwaysEnabled => "alwaysEnabled",
            AutomaticReplyStatus::Scheduled => "scheduled",
        }
    }

    /// Parses a Graph `status` value; unknown values mean disabled.
    pub fn from_graph_str(value: &str) -> Self {
        match value {
            "alwaysEnabled" => AutomaticReplyStatus::AlwaysEnabled,
            "scheduled" => AutomaticReplyStatus::Scheduled,
            _ => AutomaticReplyStatus::Disabled,
        }
    }
}

/// Who outside the organization receives a reply (Graph's `externalAudience`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ExternalAudience {
    /// External senders get no automatic reply.
    None,
    /// Only senders in the owner's contacts.
    ContactsOnly,
    /// Everyone outside the organization; Outlook's default.
    #[default]
    All,
}

impl ExternalAudience {
    /// The Graph `externalAudience` value.
    pub fn as_graph_str(self) -> &'static str {
        match self {
            ExternalAudience::None => "none",
            ExternalAudience::ContactsOnly => "contactsOnly",
            ExternalAudience::All => "all",
        }
    }

    /// Parses a Graph `externalAudience` value; unknown values mean `all`.
    pub fn from_graph_str(value: &str) -> Self {
        match value {
            "none" => ExternalAudience::None,
            "contactsOnly" => ExternalAudience::ContactsOnly,
            _ => ExternalAudience::All,
        }
    }

    /// The user-facing label (Outlook's wording).
    pub fn label(self) -> &'static str {
        match self {
            ExternalAudience::None => "No one",
            ExternalAudience::ContactsOnly => "Only contacts",
            ExternalAudience::All => "Anyone outside my organization",
        }
    }
}

/// Why a set of automatic replies can't be saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticRepliesError {
    /// `Scheduled` without both a start and an end.
    MissingSchedule,
    /// The end is not after the start.
    EndBeforeStart,
}

impl AutomaticRepliesError {
    /// The user-facing message (the settings dialog shows it on save).
    pub fn message(self) -> &'static str {
        match self {
            AutomaticRepliesError::MissingSchedule => "Choose both a start and an end time for scheduled replies.",
            AutomaticRepliesError::EndBeforeStart => "The end time must be after the start time.",
        }
    }
}

/// An account's automatic replies (Outlook's "Automatic replies" settings).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AutomaticReplies {
    #[serde(default)]
    pub status: AutomaticReplyStatus,
    /// The schedule's start; only meaningful for `Scheduled`.
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    /// The schedule's end; only meaningful for `Scheduled`.
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// The reply to people inside the organization; also the reply Sieve sends
    /// to everyone, since `vacation` has no internal/external split.
    #[serde(default)]
    pub internal_message: String,
    /// The reply to people outside the organization (Graph only).
    #[serde(default)]
    pub external_message: String,
    /// Who outside the organization gets [`AutomaticReplies::external_message`].
    #[serde(default)]
    pub external_audience: ExternalAudience,
}

impl AutomaticReplies {
    /// Whether replies are configured at all (`Disabled` is the default).
    pub fn enabled(&self) -> bool {
        self.status != AutomaticReplyStatus::Disabled
    }

    /// Whether a reply would go out at `now`: at all times for `AlwaysEnabled`,
    /// and inside `[start, end]` for a complete `Scheduled` range.
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        match self.status {
            AutomaticReplyStatus::Disabled => false,
            AutomaticReplyStatus::AlwaysEnabled => true,
            AutomaticReplyStatus::Scheduled => match (self.start, self.end) {
                (Some(start), Some(end)) => now >= start && now <= end,
                _ => false,
            },
        }
    }

    /// Checks the schedule (`Scheduled` needs both bounds, end after start).
    pub fn validate(&self) -> Result<(), AutomaticRepliesError> {
        if self.status != AutomaticReplyStatus::Scheduled {
            return Ok(());
        }
        let (Some(start), Some(end)) = (self.start, self.end) else {
            return Err(AutomaticRepliesError::MissingSchedule);
        };
        if end <= start {
            return Err(AutomaticRepliesError::EndBeforeStart);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, hour, 0, 0).unwrap()
    }

    #[test]
    fn default_is_disabled_and_inactive() {
        let replies = AutomaticReplies::default();
        assert!(!replies.enabled());
        assert!(!replies.is_active_at(at(12)));
        assert_eq!(replies.validate(), Ok(()));
    }

    #[test]
    fn always_enabled_is_active_until_turned_off() {
        let replies = AutomaticReplies {
            status: AutomaticReplyStatus::AlwaysEnabled,
            ..AutomaticReplies::default()
        };
        assert!(replies.is_active_at(at(0)));
        assert!(replies.is_active_at(at(23)));
    }

    #[test]
    fn scheduled_range_is_active_only_inside_it() {
        let replies = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            start: Some(at(9)),
            end: Some(at(17)),
            ..AutomaticReplies::default()
        };
        assert!(!replies.is_active_at(at(8)));
        assert!(replies.is_active_at(at(9)));
        assert!(replies.is_active_at(at(12)));
        assert!(replies.is_active_at(at(17)));
        assert!(!replies.is_active_at(at(18)));
    }

    #[test]
    fn scheduled_without_bounds_is_inactive_and_invalid() {
        let replies = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            ..AutomaticReplies::default()
        };
        assert!(!replies.is_active_at(at(12)));
        assert_eq!(replies.validate(), Err(AutomaticRepliesError::MissingSchedule));
    }

    #[test]
    fn end_must_be_after_start() {
        let replies = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            start: Some(at(17)),
            end: Some(at(9)),
            ..AutomaticReplies::default()
        };
        assert_eq!(replies.validate(), Err(AutomaticRepliesError::EndBeforeStart));
        let equal = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            start: Some(at(9)),
            end: Some(at(9)),
            ..AutomaticReplies::default()
        };
        assert_eq!(equal.validate(), Err(AutomaticRepliesError::EndBeforeStart));
    }

    #[test]
    fn graph_round_trip_status_and_audience() {
        for status in [AutomaticReplyStatus::Disabled, AutomaticReplyStatus::AlwaysEnabled, AutomaticReplyStatus::Scheduled] {
            assert_eq!(AutomaticReplyStatus::from_graph_str(status.as_graph_str()), status);
        }
        assert_eq!(AutomaticReplyStatus::from_graph_str("something-else"), AutomaticReplyStatus::Disabled);
        for audience in [ExternalAudience::None, ExternalAudience::ContactsOnly, ExternalAudience::All] {
            assert_eq!(ExternalAudience::from_graph_str(audience.as_graph_str()), audience);
        }
        assert_eq!(ExternalAudience::from_graph_str("something-else"), ExternalAudience::All);
    }

    #[test]
    fn serde_round_trip() {
        let replies = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            start: Some(at(9)),
            end: Some(at(17)),
            internal_message: "Away until Friday.".into(),
            external_message: "I'm away.".into(),
            external_audience: ExternalAudience::ContactsOnly,
        };
        let json = serde_json::to_string(&replies).unwrap();
        assert_eq!(serde_json::from_str::<AutomaticReplies>(&json).unwrap(), replies);
        assert_eq!(serde_json::from_str::<AutomaticReplies>("{}").unwrap(), AutomaticReplies::default());
    }
}
