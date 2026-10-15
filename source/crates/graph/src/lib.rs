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
pub mod contacts;
pub mod error;
pub mod mail;
pub mod scopes;
pub mod todo;

pub use auth::TokenProvider;
pub use calendar::client::{CalendarClient, Room};
pub use calendar::recurrence::{recurrence_from_graph, rrule_to_graph};
pub use calendar::session::run_calendar_session;
pub use calendar::{
    attendee_from_graph, attendee_to_graph, calendar_from_graph, event_create_body, event_from_graph, event_patch_body, graph_date_time, occurrence_from_graph,
    parse_graph_date_time, parse_graph_date_time_str,
};
pub use client::{delta_link_of, next_link, page_items, retry_after, DeltaPage, GraphClient, GRAPH_BASE_URL};
pub use contacts::client::ContactsClient;
pub use contacts::{contact_folder_from_graph, contact_from_graph, contact_to_graph, person_from_graph, removed_id, user_from_graph, ContactFolder, DirectoryPerson, GraphContact};
pub use error::{GraphError, Result};
pub use mail::client::MailClient;
pub use mail::{
    categories_body, classification_body, flag_body, flag_from_graph, importance_body, message_id_filter, message_properties_from_graph, outgoing_message_body, override_body,
    override_from_graph, InferenceClassification, InferenceOverride, MessageDelta, MessageProperties, OutgoingAttachment, OutgoingMessage,
};
pub use scopes::{
    GraphFeature, BASE_GRAPH_SCOPES, CALENDARS_READ_SHARED, CALENDARS_READ_WRITE, CALENDARS_READ_WRITE_SHARED, CONTACTS_READ_WRITE, MAILBOX_SETTINGS_READ_WRITE, MAIL_READ_WRITE,
    MAIL_READ_WRITE_SHARED, MAIL_SEND, PEOPLE_READ, PLACE_READ_ALL, TASKS_READ_WRITE, USER_READ_BASIC_ALL,
};
pub use todo::client::TodoClient;
pub use todo::session::{run_todo_session, TodoCommand, TodoEvent};
pub use todo::{
    checklist_item_from_graph, graph_todo_calendar_id, graph_todo_list_id, linked_resource_from_graph, task_to_todo_body, todo_list_from_graph, todo_task_from_graph,
    ChecklistStep, LinkedResource, TodoList, GRAPH_TODO_PREFIX, WELL_KNOWN_DEFAULT_LIST, WELL_KNOWN_FLAGGED_EMAIL,
};
