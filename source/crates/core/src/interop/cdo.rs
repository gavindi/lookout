//! The `X-MICROSOFT-CDO-*` calendar properties Outlook writes into iCalendar
//! data (OUTLOOK_PARITY.md §4.3).
//!
//! This pass maps the one property the calendar's "Show as" control needs:
//! `X-MICROSOFT-CDO-BUSYSTATUS` (and its invite-only twin
//! `X-MICROSOFT-CDO-INTENDEDSTATUS`), whose five values are richer than the
//! RFC 5545 `TRANSP` binary. `to_transparency`/`from_transparency` are the
//! bridge that lets non-Outlook clients see a faithful Busy/Free while Outlook
//! sees the full status.
//!
//! The remaining CDO property names are declared here so no feature code
//! hard-codes them, but their parse/emit lands with the calendar items that
//! consume them (all-day in 3.x, importance/sequence, and
//! `X-MICROSOFT-DISALLOW-COUNTER` in CAL-17).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use crate::calendar::EventTransparency;

/// `X-MICROSOFT-CDO-BUSYSTATUS` — the sender's own show-as state.
pub const X_MICROSOFT_CDO_BUSYSTATUS: &str = "X-MICROSOFT-CDO-BUSYSTATUS";
/// `X-MICROSOFT-CDO-INTENDEDSTATUS` — the organizer's intended show-as on an invite.
pub const X_MICROSOFT_CDO_INTENDEDSTATUS: &str = "X-MICROSOFT-CDO-INTENDEDSTATUS";
/// `X-MICROSOFT-CDO-ALLDAYEVENT` — `TRUE`/`FALSE`, written alongside `VALUE=DATE`.
pub const X_MICROSOFT_CDO_ALLDAYEVENT: &str = "X-MICROSOFT-CDO-ALLDAYEVENT";
/// `X-MICROSOFT-CDO-IMPORTANCE` — `0`/`1`/`2`.
pub const X_MICROSOFT_CDO_IMPORTANCE: &str = "X-MICROSOFT-CDO-IMPORTANCE";
/// `X-MICROSOFT-CDO-APPT-SEQUENCE` — Outlook's appointment sequence.
pub const X_MICROSOFT_CDO_APPT_SEQUENCE: &str = "X-MICROSOFT-CDO-APPT-SEQUENCE";
/// `X-MICROSOFT-DISALLOW-COUNTER` — `TRUE` hides "Propose new time" (CAL-17).
pub const X_MICROSOFT_DISALLOW_COUNTER: &str = "X-MICROSOFT-DISALLOW-COUNTER";

/// A calendar item's full show-as state (Graph `showAs`). Richer than RFC
/// 5545's `TRANSP`: `Tentative`, `Oof` and `WorkingElsewhere` all collapse to
/// "free" for a non-Outlook client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BusyStatus {
    Free,
    Tentative,
    #[default]
    Busy,
    /// Out of office (Graph `oof`).
    Oof,
    /// Working elsewhere (Graph `workingElsewhere`).
    WorkingElsewhere,
}

impl BusyStatus {
    /// Parses a `X-MICROSOFT-CDO-BUSYSTATUS` / `-INTENDEDSTATUS` value
    /// (`FREE`, `TENTATIVE`, `BUSY`, `OOF`, `WORKINGELSEWHERE`).
    pub fn from_cdo(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "FREE" => Some(BusyStatus::Free),
            "TENTATIVE" => Some(BusyStatus::Tentative),
            "BUSY" => Some(BusyStatus::Busy),
            "OOF" => Some(BusyStatus::Oof),
            "WORKINGELSEWHERE" => Some(BusyStatus::WorkingElsewhere),
            _ => None,
        }
    }

    /// The CDO property value for writing.
    pub fn as_cdo(self) -> &'static str {
        match self {
            BusyStatus::Free => "FREE",
            BusyStatus::Tentative => "TENTATIVE",
            BusyStatus::Busy => "BUSY",
            BusyStatus::Oof => "OOF",
            BusyStatus::WorkingElsewhere => "WORKINGELSEWHERE",
        }
    }

    /// Parses a Graph `showAs` value (`free`, `tentative`, `busy`, `oof`,
    /// `workingElsewhere`). Graph's `unknown` yields `None`.
    pub fn from_graph(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "free" => Some(BusyStatus::Free),
            "tentative" => Some(BusyStatus::Tentative),
            "busy" => Some(BusyStatus::Busy),
            "oof" => Some(BusyStatus::Oof),
            "workingelsewhere" => Some(BusyStatus::WorkingElsewhere),
            _ => None,
        }
    }

    /// Graph's `showAs` value for writing.
    pub fn as_graph_str(self) -> &'static str {
        match self {
            BusyStatus::Free => "free",
            BusyStatus::Tentative => "tentative",
            BusyStatus::Busy => "busy",
            BusyStatus::Oof => "oof",
            BusyStatus::WorkingElsewhere => "workingElsewhere",
        }
    }

    /// The RFC 5545 `TRANSP` a non-Outlook client understands: only `Free`
    /// leaves the time open, every other state blocks it.
    pub fn to_transparency(self) -> EventTransparency {
        match self {
            BusyStatus::Free => EventTransparency::Free,
            BusyStatus::Tentative | BusyStatus::Busy | BusyStatus::Oof | BusyStatus::WorkingElsewhere => EventTransparency::Busy,
        }
    }

    /// The default show-as for a given `TRANSP` when importing from a
    /// non-Outlook source (which only carries the binary form).
    pub fn from_transparency(transparency: EventTransparency) -> Self {
        match transparency {
            EventTransparency::Busy => BusyStatus::Busy,
            EventTransparency::Free => BusyStatus::Free,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdo_values_round_trip() {
        for status in [BusyStatus::Free, BusyStatus::Tentative, BusyStatus::Busy, BusyStatus::Oof, BusyStatus::WorkingElsewhere] {
            assert_eq!(BusyStatus::from_cdo(status.as_cdo()), Some(status));
        }
    }

    #[test]
    fn graph_values_round_trip() {
        for status in [BusyStatus::Free, BusyStatus::Tentative, BusyStatus::Busy, BusyStatus::Oof, BusyStatus::WorkingElsewhere] {
            assert_eq!(BusyStatus::from_graph(status.as_graph_str()), Some(status));
        }
    }

    #[test]
    fn cdo_parsing_is_case_insensitive_and_ignores_whitespace() {
        assert_eq!(BusyStatus::from_cdo(" workingelsewhere "), Some(BusyStatus::WorkingElsewhere));
        assert_eq!(BusyStatus::from_cdo("busy"), Some(BusyStatus::Busy));
        assert_eq!(BusyStatus::from_cdo("nonsense"), None);
        assert_eq!(BusyStatus::from_graph("unknown"), None);
    }

    #[test]
    fn only_free_maps_to_free_transparency() {
        assert_eq!(BusyStatus::Free.to_transparency(), EventTransparency::Free);
        for status in [BusyStatus::Tentative, BusyStatus::Busy, BusyStatus::Oof, BusyStatus::WorkingElsewhere] {
            assert_eq!(status.to_transparency(), EventTransparency::Busy);
        }
    }

    #[test]
    fn transparency_maps_back_to_the_binary_states() {
        assert_eq!(BusyStatus::from_transparency(EventTransparency::Busy), BusyStatus::Busy);
        assert_eq!(BusyStatus::from_transparency(EventTransparency::Free), BusyStatus::Free);
    }
}
