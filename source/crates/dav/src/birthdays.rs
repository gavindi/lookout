//! Birthday-event synthesis from synced contacts.
//!
//! The calendar UI renders a synthetic "Birthdays" calendar next to real
//! CalDAV calendars and webcal feeds. Unlike those two, there is nothing to
//! fetch or cache: the source data is the per-account `ContactsCache` (or,
//! equivalently, the in-memory contact snapshots the app keeps), already
//! synced by the CardDAV pipeline. This module is the pure transform in
//! between - one `Vec<EventOccurrence>` per (account, month), stamped with a
//! synthetic `CalendarId` so every existing calendar-id-keyed mechanism
//! (checklist toggles, colors, read-only editor guard) works unchanged.
//!
//! Expansion is deliberately a hand-rolled year loop rather than the
//! recurrence machinery: birthdays are simpler than general RRULEs, and the
//! loop gives exact control over the two cases that matter in practice -
//! yearless `BDAY` values (RFC 6350 `--MMDD`, or Apple's `X-APPLE-OMIT-YEAR`,
//! both of which the vCard layer flags via `Birthday::omit_year`) and
//! Feb 29 birthdays, which land on Feb 28 in non-leap years (Google and
//! Apple both do this) instead of silently vanishing.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{Datelike, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use lookout_core::{AccountId, CalendarId, EventOccurrence, EventUid, VCard};

/// Builds the birthday occurrences for one calendar month from every contact
/// of one account. Contacts without a birthday (or without any usable name)
/// are skipped; contacts whose birthday falls in the window get one
/// all-day occurrence. The result is deterministic and order-stable
/// (contacts in input order, then years ascending), so the UI diffing across
/// resyncs stays quiet.
pub fn birthday_occurrences(account_id: &AccountId, calendar_id: &CalendarId, contacts: &[crate::client::ContactRecord], month: NaiveDate) -> Vec<EventOccurrence> {
    let mut occurrences = Vec::new();
    for contact in contacts {
        let Some(name) = contact_display_name(&contact.card) else {
            continue;
        };
        let Some(birthday) = contact.card.birthday else {
            continue;
        };
        // Each year of the window gets at most one occurrence; the birthday
        // month is the anchor, so a February birthday contributes only to
        // February windows and a Dec 31 birthday only to December ones.
        let anchor = birthday.date;
        for year in month.year()..=month.year() + 1 {
            let date = year_birthday(anchor, year);
            if date.month() != month.month() || date.year() != month.year() {
                continue;
            }
            let age = if birthday.omit_year { None } else { Some(date.year() - anchor.year()) };
            let summary = if let Some(age) = age {
                if age >= 1 {
                    format!("{name}'s {} birthday", ordinal(age))
                } else {
                    format!("{name}'s birthday")
                }
            } else {
                format!("{name}'s birthday")
            };
            let start = local_midnight_utc(date);
            occurrences.push(EventOccurrence {
                uid: EventUid(format!(
                    "birthday:{}:{}",
                    account_id,
                    contact.href.trim_end_matches('/').rsplit('/').next().unwrap_or(&contact.href)
                )),
                calendar_id: calendar_id.clone(),
                summary: Some(summary),
                description: None,
                location: None,
                start,
                end: start + chrono::Duration::days(1),
                all_day: true,
                // A birthday is an annual recurrence; carrying the RRULE keeps
                // the occurrence's series semantics visible in the read-only
                // editor, which is the only editor these events ever reach.
                rrule: Some("FREQ=YEARLY".to_string()),
                recurrence_id: None,
                exdates: Vec::new(),
                master_start: None,
                master_end: None,
                href: None,
                etag: None,
                master_href: None,
                master_etag: None,
                attendees: Vec::new(),
                organizer: None,
                categories: Vec::new(),
                sensitivity: lookout_core::EventSensitivity::Public,
                transparency: lookout_core::EventTransparency::Free,
                // "At time of event": the alert fires as the birthday day
                // begins (the reminder engine computes `start - minutes` in
                // local time, so 0 = local midnight).
                reminder_minutes_before: Some(0),
                conference_url: None,
            });
        }
    }
    occurrences
}

/// The date `birthday` falls on in `year`, shifting Feb 29 to Feb 28 in
/// non-leap years.
fn year_birthday(birthday: NaiveDate, year: i32) -> NaiveDate {
    if birthday.month() == 2 && birthday.day() == 29 && !is_leap_year(year) {
        NaiveDate::from_ymd_opt(year, 2, 28).unwrap_or(birthday)
    } else {
        NaiveDate::from_ymd_opt(year, birthday.month(), birthday.day()).unwrap_or(birthday)
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// A day anchored at local midnight, expressed in UTC - the same shape the
/// iCalendar pipeline stores all-day events in, so every view that converts
/// back to local time places the occurrence on the intended day regardless of
/// the user's zone.
fn local_midnight_utc(date: NaiveDate) -> chrono::DateTime<Utc> {
    let local = date.and_time(NaiveTime::MIN);
    Local.from_local_datetime(&local).earliest().unwrap_or_else(|| Local.from_utc_datetime(&local)).into()
}

/// The contact's best display name: `FN` when present, else the structured
/// `N` joined the usual way.
fn contact_display_name(card: &VCard) -> Option<String> {
    if let Some(full) = &card.full_name {
        if !full.trim().is_empty() {
            return Some(full.clone());
        }
    }
    card.name.as_ref().map(|name| {
        let mut parts = Vec::new();
        for part in [&name.prefix, &name.given, &name.additional, &name.family, &name.suffix] {
            if !part.trim().is_empty() {
                parts.push(part.clone());
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            parts.join(" ")
        }
    })
}

fn ordinal(n: i32) -> String {
    let suffix = match n % 100 {
        11..=13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{Birthday, VCard};

    fn record(name: &str, birthday: Option<Birthday>) -> crate::client::ContactRecord {
        crate::client::ContactRecord {
            href: format!("/books/1/{}.vcf", name.to_lowercase()),
            etag: None,
            card: VCard {
                version: "4.0".to_string(),
                kind: None,
                uid: Some(name.to_string()),
                full_name: Some(name.to_string()),
                name: None,
                organization: None,
                title: None,
                emails: Vec::new(),
                telephones: Vec::new(),
                addresses: Vec::new(),
                urls: Vec::new(),
                note: None,
                birthday,
                categories: Vec::new(),
                other: Vec::new(),
            },
        }
    }

    fn bday(y: i32, m: u32, d: u32, omit_year: bool) -> Birthday {
        Birthday {
            date: NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            omit_year,
        }
    }

    #[test]
    fn emits_all_day_occurrence_only_in_the_birthday_month() {
        let contacts = vec![record("Alice", Some(bday(1990, 5, 23, false)))];
        let june = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        );
        assert!(june.is_empty(), "a May birthday must not appear in June");
        let may = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        );
        assert_eq!(may.len(), 1);
        let occ = &may[0];
        assert_eq!(occ.summary.as_deref(), Some("Alice's 36th birthday"));
        assert!(occ.all_day);
        assert_eq!(occ.start.with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(2026, 5, 23).unwrap());
        assert_eq!(occ.end - occ.start, chrono::Duration::days(1));
        assert_eq!(occ.rrule.as_deref(), Some("FREQ=YEARLY"));
        assert_eq!(occ.reminder_minutes_before, Some(0));
        assert_eq!(occ.uid.0, "birthday:a1:alice.vcf");
    }

    #[test]
    fn december_birthday_stays_in_december_across_the_year_boundary() {
        let contacts = vec![record("Noel", Some(bday(1980, 12, 31, false)))];
        for (month, expected) in [
            (NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), 0),
            (NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(), 1),
            (NaiveDate::from_ymd_opt(2027, 1, 1).unwrap(), 0),
        ] {
            let occurrences = birthday_occurrences(&AccountId("a1".into()), &CalendarId("birthdays".into()), &contacts, month);
            assert_eq!(occurrences.len(), expected, "{month}");
        }
    }

    #[test]
    fn yearless_birthdays_recur_every_year_without_an_age() {
        let contacts = vec![record("Mystery", Some(bday(2000, 7, 4, true)))];
        for year in 2025..=2027 {
            let occurrences = birthday_occurrences(
                &AccountId("a1".into()),
                &CalendarId("birthdays".into()),
                &contacts,
                NaiveDate::from_ymd_opt(year, 7, 1).unwrap(),
            );
            assert_eq!(occurrences.len(), 1, "{year}");
            assert_eq!(occurrences[0].summary.as_deref(), Some("Mystery's birthday"));
            assert_eq!(occurrences[0].start.with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(year, 7, 4).unwrap());
        }
    }

    #[test]
    fn feb_29_birthdays_shift_to_feb_28_in_non_leap_years() {
        let contacts = vec![record("Leap", Some(bday(1992, 2, 29, false)))];
        let leap = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(),
        );
        assert_eq!(leap[0].start.with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(2024, 2, 29).unwrap());
        let plain = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2023, 2, 1).unwrap(),
        );
        assert_eq!(plain[0].start.with_timezone(&Local).date_naive(), NaiveDate::from_ymd_opt(2023, 2, 28).unwrap());
        let century = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2100, 2, 1).unwrap(),
        );
        assert_eq!(
            century[0].start.with_timezone(&Local).date_naive(),
            NaiveDate::from_ymd_opt(2100, 2, 28).unwrap(),
            "2100 is not a leap year"
        );
    }

    #[test]
    fn skips_contacts_without_birthday_or_name() {
        let contacts = vec![
            record("No Birthday", None),
            crate::client::ContactRecord {
                href: "nobday.vcf".to_string(),
                etag: None,
                card: VCard {
                    version: "4.0".to_string(),
                    kind: None,
                    uid: Some("x".to_string()),
                    full_name: Some("  ".to_string()),
                    name: None,
                    organization: None,
                    title: None,
                    emails: Vec::new(),
                    telephones: Vec::new(),
                    addresses: Vec::new(),
                    urls: Vec::new(),
                    note: None,
                    birthday: Some(bday(1990, 1, 1, false)),
                    categories: Vec::new(),
                    other: Vec::new(),
                },
            },
        ];
        let occurrences = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        );
        assert!(occurrences.is_empty());
    }

    #[test]
    fn uses_structured_name_when_full_name_is_absent() {
        let mut contact = record("N/A", Some(bday(1990, 3, 5, true)));
        contact.card.full_name = None;
        contact.card.name = Some(lookout_core::Name {
            family: "Lovelace".to_string(),
            given: "Ada".to_string(),
            additional: "A".to_string(),
            prefix: "Dr".to_string(),
            suffix: String::new(),
        });
        let occurrences = birthday_occurrences(
            &AccountId("a1".into()),
            &CalendarId("birthdays".into()),
            &[contact],
            NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
        );
        assert_eq!(occurrences[0].summary.as_deref(), Some("Dr Ada A Lovelace's birthday"));
    }

    #[test]
    fn uids_are_stable_and_account_scoped() {
        let contacts = vec![record("Alice", Some(bday(1990, 5, 23, false)))];
        let a = birthday_occurrences(
            &AccountId("acct-a".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        );
        let b = birthday_occurrences(
            &AccountId("acct-b".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2027, 5, 1).unwrap(),
        );
        assert_ne!(a[0].uid, b[0].uid);
        assert_eq!(a[0].uid.0, "birthday:acct-a:alice.vcf");
        let again = birthday_occurrences(
            &AccountId("acct-a".into()),
            &CalendarId("birthdays".into()),
            &contacts,
            NaiveDate::from_ymd_opt(2027, 5, 1).unwrap(),
        );
        assert_eq!(again[0].uid, a[0].uid, "the uid must not change year to year");
    }
}
