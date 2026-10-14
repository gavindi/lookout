//! Bundled public-holiday calendar feeds (OUTLOOK_PARITY.md§3.15 / CAL-28).
//!
//! The app's "Add calendar" dialog offers a country picker that subscribes to
//! a country's public holidays through the existing webcal path (CAL-26). The
//! country table is a compiled-in JSON file (`source/data/holidays.json`) of
//! Google public holiday calendars: each row pairs an ISO 3166-1 alpha-2 code
//! and display name with the Google Calendar id fragment (`feed`, e.g.
//! `en.usa`). The full feed URL is built here, so no feature code has to know
//! the Google URL shape.
//!
//! Like the rest of `lookout-core` this is I/O-free: it derives strings and
//! [`WebcalSubscription`]s, never fetches anything.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::OnceLock;

use serde::Deserialize;

use crate::calendar::WebcalSubscription;

/// The bundled table, embedded at compile time.
const HOLIDAYS_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/holidays.json"));

/// The id every Google public holiday calendar hangs its `.ics` off.
const GOOGLE_HOLIDAY_SUFFIX: &str = "#holiday@group.v.calendar.google.com";

#[derive(Debug, Deserialize)]
struct HolidayCountryRow {
    code: String,
    name: String,
    feed: String,
}

/// One bundled country: its ISO 3166-1 alpha-2 `code`, the display `name`, and
/// the Google Calendar id fragment (`feed`, e.g. `"en.usa"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolidayCountry {
    pub code: String,
    pub name: String,
    pub feed: String,
}

impl HolidayCountry {
    /// The country's public `.ics` URL, with the id's `#`/`@` percent-encoded
    /// for the path.
    pub fn feed_url(&self) -> String {
        let id = format!("{}{GOOGLE_HOLIDAY_SUFFIX}", self.feed);
        let encoded = id.replace('#', "%23").replace('@', "%40");
        format!("https://calendar.google.com/calendar/ical/{encoded}/public/basic.ics")
    }

    /// A ready-to-persist subscription, named as Outlook names a holiday
    /// calendar ("Holidays (United States)").
    pub fn to_subscription(&self) -> WebcalSubscription {
        WebcalSubscription {
            id: uuid::Uuid::new_v4().to_string(),
            display_name: format!("Holidays ({})", self.name),
            url: self.feed_url(),
        }
    }
}

/// The bundled country list, sorted by display name. Parsing the compiled-in
/// constant is infallible in practice (a malformed file fails the build's
/// tests), so callers get a plain slice.
pub fn holiday_countries() -> &'static [HolidayCountry] {
    static CACHE: OnceLock<Vec<HolidayCountry>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let rows: Vec<HolidayCountryRow> = serde_json::from_str(HOLIDAYS_JSON).expect("bundled holidays.json is valid");
        let mut countries: Vec<HolidayCountry> = rows
            .into_iter()
            .map(|row| HolidayCountry {
                code: row.code,
                name: row.name,
                feed: row.feed,
            })
            .collect();
        countries.sort_by(|a, b| a.name.cmp(&b.name));
        countries
    })
}

/// Whether `url` already matches one of `subscriptions` (case-insensitive,
/// ignoring a trailing slash) - the picker's guard against subscribing twice.
pub fn is_holiday_subscribed(url: &str, subscriptions: &[WebcalSubscription]) -> bool {
    let normalize = |value: &str| value.trim().trim_end_matches('/').to_ascii_lowercase();
    let target = normalize(url);
    subscriptions.iter().any(|subscription| normalize(&subscription.url) == target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_parses_and_is_broad() {
        let countries = holiday_countries();
        assert!(countries.len() >= 40, "the bundled list should be broad, got {}", countries.len());
    }

    #[test]
    fn countries_are_sorted_and_unique() {
        let countries = holiday_countries();
        assert!(countries.windows(2).all(|pair| pair[0].name <= pair[1].name), "list must be sorted by name");
        let mut codes: Vec<&str> = countries.iter().map(|c| c.code.as_str()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), countries.len(), "country codes must be unique");
    }

    #[test]
    fn feed_url_percent_encodes_the_google_id() {
        let usa = holiday_countries().iter().find(|c| c.code == "US").expect("US present");
        assert_eq!(
            usa.feed_url(),
            "https://calendar.google.com/calendar/ical/en.usa%23holiday%40group.v.calendar.google.com/public/basic.ics"
        );
    }

    #[test]
    fn subscription_is_named_for_outlook() {
        let germany = holiday_countries().iter().find(|c| c.code == "DE").expect("DE present");
        let subscription = germany.to_subscription();
        assert_eq!(subscription.display_name, "Holidays (Germany)");
        assert!(subscription.url.contains("de.german%23holiday%40group.v.calendar.google.com"));
        assert!(!subscription.id.is_empty());
    }

    #[test]
    fn subscribed_matching_ignores_case_and_trailing_slash() {
        let existing = vec![WebcalSubscription {
            id: "x".to_string(),
            display_name: "Holidays (United States)".to_string(),
            url: "https://calendar.google.com/calendar/ical/en.usa%23holiday%40group.v.calendar.google.com/public/basic.ics/".to_string(),
        }];
        assert!(is_holiday_subscribed(
            "HTTPS://calendar.google.com/calendar/ical/en.usa%23holiday%40group.v.calendar.google.com/public/basic.ics",
            &existing
        ));
        assert!(!is_holiday_subscribed("https://example.com/other.ics", &existing));
    }
}
