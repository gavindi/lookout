//! The webcal subscription session actor: the fetch-only cousin of
//! [`crate::session::run_calendar_session`]. One task polls every configured
//! feed on a fixed cadence (webcal has no push protocol and feeds aren't
//! CalDAV collections, so there's no `sync-collection` to lean on), parsing
//! each response through the same iCalendar path the CalDAV session uses and
//! emitting per-feed occurrence buckets for the displayed month.
//!
//! Feeds are read-only by design (there is no write-back protocol), so this
//! session handles no create/update/delete commands - the UI's write paths
//! simply never target a subscription's `CalendarId`. One feed failing (or a
//! subscription URL that was valid at add-time but is dead now) must not
//! blank out the other feeds: each feed's result carries its own
//! `Option<String>` error, and its last-good occurrences stay visible while
//! it's down (stale-while-revalidate, same convention as the CalDAV cache).

use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveTime};
use lookout_core::{AccountId, CalendarId, EventOccurrence, WebcalSubscription};

use crate::cache::CalendarCache;
use crate::client::{fetch_webcal_ics, normalize_webcal_url};
use crate::recurrence;

/// Poll cadence for feeds - same fixed-interval philosophy as the CalDAV
/// session's `POLL_INTERVAL`.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug)]
pub enum SubscriptionCommand {
    /// Any date within the month to display; the actor resolves the month's
    /// own bounds itself.
    SyncMonth(NaiveDate),
    /// Fetch every feed's occurrences for one month without making it the
    /// polled month - the Lookout dashboard's next-month horizon, which
    /// must not hijack the feed session's own displayed month.
    FetchMonth(NaiveDate),
    /// Force a re-fetch of every feed outside the poll cadence.
    Refresh,
    /// The subscription list changed (added or removed) - adopt this full
    /// list and re-sync. The list lives in the app's `settings.json`, so the
    /// session never mutates it itself.
    Reload {
        subscriptions: Vec<WebcalSubscription>,
    },
    Shutdown,
}

/// One feed's sync result for the displayed month. Always one per configured
/// subscription, in list order - never fewer, so the UI can keep per-feed
/// state (checked-ness, colors, last-good occurrences) aligned by id.
#[derive(Debug)]
pub struct SubscriptionFeed {
    /// The subscription's stable id (matches `WebcalSubscription::id`).
    pub subscription_id: String,
    /// The synthetic calendar id (`"webcal:<id>"`) this feed's events are
    /// stamped with, so the UI's calendar-id-keyed machinery (checklist
    /// toggles, color map, checked set) works unchanged.
    pub calendar_id: CalendarId,
    /// Expanded occurrences for the month. Empty - not an error - when the
    /// feed simply has no events in the window.
    pub occurrences: Vec<EventOccurrence>,
    /// Set when this feed's fetch or parse failed this round. `occurrences`
    /// then holds the last-good cached data, if any, so the feed's events
    /// don't vanish during an outage.
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum SubscriptionSessionEvent {
    /// One [`SubscriptionFeed`] per configured subscription.
    SubscriptionsUpdated { month: NaiveDate, feeds: Vec<SubscriptionFeed> },
}

/// Runs the feed-polling lifecycle on the calling task (spawn onto the shared
/// tokio worker thread, same as `run_calendar_session`). No backoff loop is
/// needed: a failed fetch is reported per-feed and the next poll retries it.
pub async fn run_subscription_session(
    initial: Vec<WebcalSubscription>,
    commands: async_channel::Receiver<SubscriptionCommand>,
    events: async_channel::Sender<SubscriptionSessionEvent>,
) {
    let http = reqwest::Client::new();
    let mut subscriptions = initial;
    let mut current_month = first_of_month(chrono::Utc::now().date_naive());

    // Fast first paint: whatever each feed's cache holds for the current
    // month, emitted before any network activity; superseded by the live
    // sync below (see `CalendarCache`'s doc comment for why it's never
    // authoritative).
    let mut cached_feeds = Vec::new();
    for subscription in &subscriptions {
        let feed = cached_feed(subscription, current_month);
        cached_feeds.push(feed);
    }
    let _ = events
        .send(SubscriptionSessionEvent::SubscriptionsUpdated {
            month: current_month,
            feeds: cached_feeds,
        })
        .await;
    sync_all(&http, &subscriptions, current_month, &events).await;

    loop {
        enum Wake {
            Poll,
            Command(SubscriptionCommand),
            ChannelClosed,
        }
        let wake = tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => Wake::Poll,
            c = commands.recv() => match c {
                Ok(cmd) => Wake::Command(cmd),
                Err(_) => Wake::ChannelClosed,
            },
        };

        let mut woke_on_command = None;
        match wake {
            Wake::Poll => sync_all(&http, &subscriptions, current_month, &events).await,
            Wake::Command(cmd) => woke_on_command = Some(cmd),
            Wake::ChannelClosed => return,
        }

        for command in woke_on_command.into_iter().chain(std::iter::from_fn(|| commands.try_recv().ok())) {
            match command {
                SubscriptionCommand::Shutdown => return,
                SubscriptionCommand::Refresh => sync_all(&http, &subscriptions, current_month, &events).await,
                SubscriptionCommand::SyncMonth(date) => {
                    current_month = first_of_month(date);
                    sync_all(&http, &subscriptions, current_month, &events).await;
                }
                SubscriptionCommand::FetchMonth(date) => {
                    sync_all(&http, &subscriptions, first_of_month(date), &events).await;
                }
                SubscriptionCommand::Reload { subscriptions: new_list } => {
                    subscriptions = new_list;
                    sync_all(&http, &subscriptions, current_month, &events).await;
                }
            }
        }
    }
}

fn cached_feed(subscription: &WebcalSubscription, month: NaiveDate) -> SubscriptionFeed {
    let cache = CalendarCache::open(&AccountId(format!("webcal-{}", subscription.id))).ok();
    let occurrences = cache.and_then(|c| c.load_month(month).ok()).flatten().unwrap_or_default();
    SubscriptionFeed {
        subscription_id: subscription.id.clone(),
        calendar_id: CalendarId(format!("webcal:{}", subscription.id)),
        occurrences,
        error: None,
    }
}

async fn sync_all(http: &reqwest::Client, subscriptions: &[WebcalSubscription], month: NaiveDate, events: &async_channel::Sender<SubscriptionSessionEvent>) {
    let mut feeds = Vec::with_capacity(subscriptions.len());
    for subscription in subscriptions {
        feeds.push(sync_feed(http, subscription, month).await);
    }
    let _ = events.send(SubscriptionSessionEvent::SubscriptionsUpdated { month, feeds }).await;
}

/// Fetches + parses one feed for the month window: serve the cache first
/// (stale-while-revalidate, see `cached_feed`'s callers), then fetch; a
/// successful fetch expands occurrences into the exact month window and
/// re-stores the cache, a failure keeps the cached data (if any) and records
/// the error on the feed. Expansion reuses the CalDAV session's recurrence
/// machinery, so feed RRULEs render identically to CalDAV ones.
async fn sync_feed(http: &reqwest::Client, subscription: &WebcalSubscription, month: NaiveDate) -> SubscriptionFeed {
    let cache = CalendarCache::open(&AccountId(format!("webcal-{}", subscription.id))).ok();
    let cached = cache.as_ref().and_then(|c| c.load_month(month).ok()).flatten();

    let url = match normalize_webcal_url(&subscription.url) {
        Ok(url) => url,
        Err(e) => {
            let error = e.to_string();
            tracing::warn!("invalid webcal feed URL {:?}: {e}", subscription.url);
            return SubscriptionFeed {
                subscription_id: subscription.id.clone(),
                calendar_id: CalendarId(format!("webcal:{}", subscription.id)),
                occurrences: cached.unwrap_or_default(),
                error: Some(error),
            };
        }
    };

    match fetch_webcal_ics(http, &url).await {
        Ok(ics) => {
            let calendar_id = CalendarId(format!("webcal:{}", subscription.id));
            let month_end = next_month(month);
            let window_start = month.and_time(NaiveTime::MIN).and_utc();
            let window_end = month_end.and_time(NaiveTime::MIN).and_utc();
            let occurrences: Vec<EventOccurrence> = crate::ical::parse_vevents(&calendar_id, &ics)
                .into_iter()
                .flat_map(|event| recurrence::expand_occurrences(&event, window_start, window_end))
                .collect();
            if let Some(cache) = &cache {
                if let Err(e) = cache.store_month(month, &occurrences) {
                    tracing::warn!("failed to cache webcal occurrences for {month}: {e}");
                }
            }
            SubscriptionFeed {
                subscription_id: subscription.id.clone(),
                calendar_id,
                occurrences,
                error: None,
            }
        }
        Err(e) => {
            let error = e.to_string();
            tracing::warn!("failed to fetch webcal feed {}: {e}", subscription.url);
            SubscriptionFeed {
                subscription_id: subscription.id.clone(),
                calendar_id: CalendarId(format!("webcal:{}", subscription.id)),
                occurrences: cached.unwrap_or_default(),
                error: Some(error),
            }
        }
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

fn next_month(date: NaiveDate) -> NaiveDate {
    if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap_or(date)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1).unwrap_or(date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn first_of_month_normalizes_any_day() {
        assert_eq!(first_of_month(NaiveDate::from_ymd_opt(2026, 7, 17).unwrap()), NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }

    #[test]
    fn next_month_wraps_december_into_next_year() {
        assert_eq!(next_month(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()), NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    fn ics_for(day: u32) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:feed-event-1\r\nDTSTAMP:20260801T000000Z\r\nDTSTART:202608{:02}T090000Z\r\nDTEND:202608{:02}T100000Z\r\nSUMMARY:Feed event\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            day, day,
        )
    }

    /// The session's full lifecycle against a local feed server: fast-paint
    /// (empty - no cache yet) then the live fetch, parsed and expanded into
    /// the current month's window.
    #[tokio::test]
    async fn subscription_session_fetches_parses_and_caches_feed_occurrences() {
        // A stale cache from a previous (possibly failed) run would show up in
        // the fast-paint assertion below - start clean.
        let _ = crate::cache::remove_subscription_cache("sub-1");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/feed.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ics_for(15)))
            .mount(&server)
            .await;

        let subscription = WebcalSubscription {
            id: "sub-1".to_string(),
            display_name: "Test feed".to_string(),
            url: format!("{}/feed.ics", server.uri()),
        };
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let session = tokio::spawn(run_subscription_session(vec![subscription], cmd_rx, evt_tx));

        // 1: the fast first paint (nothing cached -> empty, no error).
        let SubscriptionSessionEvent::SubscriptionsUpdated { month: _, feeds } = evt_rx.recv().await.unwrap();
        assert_eq!(feeds.len(), 1);
        assert!(feeds[0].occurrences.is_empty());
        assert!(feeds[0].error.is_none());
        assert_eq!(feeds[0].calendar_id.0, "webcal:sub-1");

        // 2: the live sync - the feed event must land in the current month
        // window (the fixture's day 15 is mid-month, so expansion keeps it).
        let SubscriptionSessionEvent::SubscriptionsUpdated { month, feeds } = evt_rx.recv().await.unwrap();
        assert_eq!(feeds.len(), 1);
        assert!(feeds[0].error.is_none(), "feed should fetch cleanly");
        assert_eq!(feeds[0].occurrences.len(), 1);
        assert_eq!(feeds[0].occurrences[0].uid.0, "feed-event-1");
        assert_eq!(feeds[0].occurrences[0].calendar_id.0, "webcal:sub-1");
        assert_eq!(feeds[0].occurrences[0].summary.as_deref(), Some("Feed event"));
        assert!(feeds[0].occurrences[0].start.date_naive() >= first_of_month(month));
        assert!(feeds[0].occurrences[0].start.date_naive() < next_month(month));

        // 3: a manual Refresh re-fetches and re-emits.
        cmd_tx.send(SubscriptionCommand::Refresh).await.unwrap();
        let SubscriptionSessionEvent::SubscriptionsUpdated { .. } = evt_rx.recv().await.unwrap();

        cmd_tx.send(SubscriptionCommand::Shutdown).await.unwrap();
        session.await.unwrap();

        // Clean up the cache file the session wrote.
        let _ = crate::cache::remove_subscription_cache("sub-1");
    }

    /// A failing feed must not take the session down: the feed reports its
    /// error (with last-good data, i.e. empty here) and polling continues.
    #[tokio::test]
    async fn subscription_session_reports_feed_errors_without_stopping() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/dead.ics"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let subscription = WebcalSubscription {
            id: "sub-dead".to_string(),
            display_name: "Dead feed".to_string(),
            url: format!("{}/dead.ics", server.uri()),
        };
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let session = tokio::spawn(run_subscription_session(vec![subscription], cmd_rx, evt_tx));

        // Fast paint first, then the failing live sync.
        let _ = evt_rx.recv().await.unwrap();
        let SubscriptionSessionEvent::SubscriptionsUpdated { feeds, .. } = evt_rx.recv().await.unwrap();
        assert_eq!(feeds.len(), 1);
        assert!(feeds[0].error.is_some(), "the feed's HTTP 500 must surface as a per-feed error");
        assert!(feeds[0].occurrences.is_empty());

        cmd_tx.send(SubscriptionCommand::Shutdown).await.unwrap();
        session.await.unwrap();
    }
}
