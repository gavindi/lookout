//! The calendar's display time zone (item 3.1 / CAL-10): the zone every
//! calendar surface - the grids, agenda, mini calendars, My Day, the event
//! editor and reminders - shows and edits times in. Settings → Calendar →
//! "Time zone" picks it; empty means the system zone.
//!
//! All-day occurrences are deliberately zone-independent: they are encoded as
//! UTC midnight of their calendar dates (the shape `lookout-dav`'s iCalendar
//! parser produces from `VALUE=DATE`), so [`DisplayZone::occurrence_to_local`]
//! reads their dates straight off the UTC value and a zone change can never
//! move a birthday or a holiday to another day.

use std::sync::RwLock;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc};

/// The zone calendar times are shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayZone {
    /// The system zone (`chrono::Local`).
    #[default]
    System,
    Named(chrono_tz::Tz),
}

/// The current display zone, set from the Calendar preferences at startup
/// and whenever they change (see `window::apply_calendar_preferences`).
/// Process-wide rather than per-thread, so calendar code that runs off the UI
/// thread (the assistant's tools) sees the same zone. Tests never set it -
/// they pass explicit `DisplayZone`s - so parallel tests can't interfere.
static CURRENT: RwLock<DisplayZone> = RwLock::new(DisplayZone::System);

/// The display zone calendar surfaces render in.
pub fn display_zone() -> DisplayZone {
    *CURRENT.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn set_display_zone(zone: DisplayZone) {
    *CURRENT.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = zone;
}

/// The system zone's IANA name (e.g. "Australia/Sydney"), when it can be
/// determined.
pub fn system_zone_name() -> Option<String> {
    iana_time_zone::get_timezone().ok().filter(|name| !name.is_empty())
}

/// Every IANA zone name the time-zone pickers offer, sorted - the Calendar
/// preferences' two pickers (3.1) and the event editor's per-event zone
/// (CAL-11) share the one list.
pub fn iana_zone_names() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<&'static str> = chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect();
        names.sort_unstable();
        names
    })
}

/// Resolves a wall-clock time in `tz` to an instant without panicking: an
/// ambiguous time (a DST fall-back) takes the earlier offset, and a time in a
/// gap (a spring-forward) is read with the offset in effect just before it -
/// so 02:30 on a 02:00→03:00 night lands on the instant 03:30 names.
fn resolve<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(earlier, _) => earlier.with_timezone(&Utc),
        chrono::LocalResult::None => {
            let before = tz
                .offset_from_local_datetime(&(naive - Duration::hours(6)))
                .earliest()
                .map(|o| o.fix().local_minus_utc())
                .unwrap_or(0);
            (naive - Duration::seconds(i64::from(before))).and_utc()
        }
    }
}

impl DisplayZone {
    /// The zone named by a stored preference (`None`/unknown = system).
    pub fn from_setting(tz: Option<chrono_tz::Tz>) -> Self {
        tz.map_or(DisplayZone::System, DisplayZone::Named)
    }

    /// The named zone this resolves to: the configured one, or the system
    /// zone's IANA entry when it's known.
    pub fn tz(self) -> Option<chrono_tz::Tz> {
        match self {
            DisplayZone::Named(tz) => Some(tz),
            DisplayZone::System => system_zone_name().and_then(|name| name.parse().ok()),
        }
    }

    /// `at` as wall-clock time in this zone.
    pub fn to_local(self, at: DateTime<Utc>) -> NaiveDateTime {
        match self {
            DisplayZone::System => at.with_timezone(&chrono::Local).naive_local(),
            DisplayZone::Named(tz) => at.with_timezone(&tz).naive_local(),
        }
    }

    /// A wall-clock time in this zone as an instant (see [`resolve`]).
    pub fn resolve_local(self, naive: NaiveDateTime) -> DateTime<Utc> {
        match self {
            DisplayZone::System => resolve(&chrono::Local, naive),
            DisplayZone::Named(tz) => resolve(&tz, naive),
        }
    }

    pub fn now(self) -> NaiveDateTime {
        self.to_local(Utc::now())
    }

    pub fn today(self) -> NaiveDate {
        self.now().date()
    }

    /// An occurrence boundary as shown in the calendar: all-day values are
    /// UTC-midnight dates (read as-is), timed ones convert into this zone.
    pub fn occurrence_to_local(self, at: DateTime<Utc>, all_day: bool) -> NaiveDateTime {
        if all_day {
            at.naive_utc()
        } else {
            self.to_local(at)
        }
    }

    /// The inverse of [`Self::occurrence_to_local`]: an all-day boundary
    /// becomes UTC midnight of its date, a timed one resolves in this zone.
    pub fn occurrence_from_local(self, naive: NaiveDateTime, all_day: bool) -> DateTime<Utc> {
        if all_day {
            naive.date().and_time(NaiveTime::MIN).and_utc()
        } else {
            self.resolve_local(naive)
        }
    }

    /// The instant an occurrence boundary happens: a timed one is already
    /// that instant; an all-day one (a UTC-midnight date) begins at 00:00 of
    /// its date in this zone - what a reminder "15 minutes before" an
    /// all-day event counts back from.
    pub fn occurrence_instant(self, at: DateTime<Utc>, all_day: bool) -> DateTime<Utc> {
        if all_day {
            self.resolve_local(at.naive_utc())
        } else {
            at
        }
    }

    /// The zone's offset from UTC at `at`, in seconds.
    pub fn offset_seconds(self, at: DateTime<Utc>) -> i32 {
        match self {
            DisplayZone::System => at.with_timezone(&chrono::Local).offset().fix().local_minus_utc(),
            DisplayZone::Named(tz) => at.with_timezone(&tz).offset().fix().local_minus_utc(),
        }
    }

    /// A short label for the zone at `at`: its abbreviation ("AEDT"), or the
    /// numeric offset ("UTC+10") for zones whose abbreviation is itself
    /// numeric or when the system zone's name is unknown.
    pub fn label(self, at: DateTime<Utc>) -> String {
        if let Some(tz) = self.tz() {
            let abbreviation = at.with_timezone(&tz).format("%Z").to_string();
            if abbreviation.chars().any(|c| c.is_ascii_alphabetic()) {
                return abbreviation;
            }
        }
        offset_label(self.offset_seconds(at))
    }
}

/// "UTC", "UTC+10", "UTC-3:30".
pub fn offset_label(seconds: i32) -> String {
    if seconds == 0 {
        return "UTC".to_string();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    match minutes % 60 {
        0 => format!("UTC{sign}{}", minutes / 60),
        m => format!("UTC{sign}{}:{m:02}", minutes / 60),
    }
}

/// The display span of an occurrence in `zone` (see
/// [`DisplayZone::occurrence_to_local`]).
pub fn occurrence_span(zone: DisplayZone, start: DateTime<Utc>, end: DateTime<Utc>, all_day: bool) -> (NaiveDateTime, NaiveDateTime) {
    (zone.occurrence_to_local(start, all_day), zone.occurrence_to_local(end, all_day))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()
    }

    #[test]
    fn named_zone_round_trips_wall_clock_time() {
        let zone = DisplayZone::Named(chrono_tz::Asia::Kolkata);
        let at = zone.resolve_local(naive(2026, 10, 12, 9, 0));
        assert_eq!(at, naive(2026, 10, 12, 3, 30).and_utc());
        assert_eq!(zone.to_local(at), naive(2026, 10, 12, 9, 0));
    }

    /// Europe/Berlin springs forward 02:00→03:00 on 29 Mar 2026 and falls
    /// back 03:00→02:00 on 25 Oct 2026.
    #[test]
    fn dst_gaps_and_overlaps_resolve_deterministically() {
        let zone = DisplayZone::Named(chrono_tz::Europe::Berlin);
        // 02:30 doesn't exist: read with the pre-transition +1 offset.
        assert_eq!(zone.resolve_local(naive(2026, 3, 29, 2, 30)), naive(2026, 3, 29, 1, 30).and_utc());
        // 02:30 happens twice: the earlier (+2) instant wins.
        assert_eq!(zone.resolve_local(naive(2026, 10, 25, 2, 30)), naive(2026, 10, 25, 0, 30).and_utc());
    }

    /// All-day boundaries never move with the zone - in UTC+14 or UTC-11 a
    /// 13 Oct all-day event still reads as 13 Oct, and converts back to UTC
    /// midnight of that date.
    #[test]
    fn all_day_boundaries_are_zone_independent() {
        let start = naive(2026, 10, 13, 0, 0).and_utc();
        for zone in [
            DisplayZone::Named(chrono_tz::Pacific::Kiritimati),
            DisplayZone::Named(chrono_tz::Pacific::Pago_Pago),
            DisplayZone::System,
        ] {
            let local = zone.occurrence_to_local(start, true);
            assert_eq!(local, naive(2026, 10, 13, 0, 0), "{zone:?}");
            assert_eq!(zone.occurrence_from_local(local, true), start, "{zone:?}");
        }
        // ...and an all-day event happens from local midnight of its date.
        let berlin = DisplayZone::Named(chrono_tz::Europe::Berlin);
        assert_eq!(berlin.occurrence_instant(start, true), naive(2026, 10, 12, 22, 0).and_utc());
        assert_eq!(berlin.occurrence_instant(start, false), start);
        // A timed event does move: 23:00 UTC is the next day in UTC+14.
        let late = naive(2026, 10, 13, 23, 0).and_utc();
        assert_eq!(
            DisplayZone::Named(chrono_tz::Pacific::Kiritimati).occurrence_to_local(late, false),
            naive(2026, 10, 14, 13, 0)
        );
    }

    #[test]
    fn labels_prefer_abbreviations_then_offsets() {
        let summer = naive(2026, 7, 1, 12, 0).and_utc();
        assert_eq!(DisplayZone::Named(chrono_tz::Europe::Berlin).label(summer), "CEST");
        assert_eq!(offset_label(0), "UTC");
        assert_eq!(offset_label(10 * 3600), "UTC+10");
        assert_eq!(offset_label(-(3 * 3600 + 1800)), "UTC-3:30");
    }
}
