//! Microsoft Graph client skeleton for Lookout (OUTLOOK_PARITY.md item 0.7).
//!
//! This is the transport every Graph-backed feature builds on: a small
//! `reqwest` client that attaches a bearer token from a [`TokenProvider`],
//! follows `@odata.nextLink` paging, reads `/delta` cursors, honours the
//! `Retry-After` throttling header, and normalises Graph's errors into
//! [`GraphError`]. The calendar, people, tasks and mail-property features
//! (Phase 5) layer the `lookout-dav`-shaped actor contract on top of it.
//!
//! The token itself comes from the platform: the app implements
//! [`TokenProvider`] against `microsoft_oauth.rs` (`TokenAudience::Graph`), the
//! same way `lookout-mail` takes credentials through `CredentialProvider`, so
//! this crate has no OAuth or keyring dependency of its own.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

pub mod auth;
pub mod calendar;
pub mod client;
pub mod error;
pub mod scopes;

pub use auth::TokenProvider;
pub use calendar::client::CalendarClient;
pub use calendar::recurrence::{recurrence_from_graph, rrule_to_graph};
pub use calendar::session::run_calendar_session;
pub use calendar::{
    attendee_from_graph, attendee_to_graph, calendar_from_graph, event_create_body, event_from_graph, event_patch_body, graph_date_time, occurrence_from_graph,
    parse_graph_date_time, parse_graph_date_time_str,
};
pub use client::{delta_link_of, next_link, page_items, retry_after, DeltaPage, GraphClient, GRAPH_BASE_URL};
pub use error::{GraphError, Result};
pub use scopes::{
    GraphFeature, BASE_GRAPH_SCOPES, CALENDARS_READ_SHARED, CALENDARS_READ_WRITE, CALENDARS_READ_WRITE_SHARED, CONTACTS_READ_WRITE, MAILBOX_SETTINGS_READ_WRITE, MAIL_READ_WRITE,
    MAIL_READ_WRITE_SHARED, MAIL_SEND, PEOPLE_READ, PLACE_READ_ALL, TASKS_READ_WRITE, USER_READ_BASIC_ALL,
};
