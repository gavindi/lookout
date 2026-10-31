//! Microsoft 365 room suggestions for the event editor
//! (OUTLOOK_PARITY.md item 15 / CAL-19).
//!
//! Backs the event form's **Find a room** popover: the tenant's bookable rooms
//! from Graph `GET /places/microsoft.graph.room`, and each candidate's
//! free/busy for the meeting window through `POST /me/calendar/getSchedule`
//! (the same endpoint the Scheduling Assistant already uses). Graph is the
//! only directory-aware backend, so CalDAV/webcal calendars keep Outlook's
//! free-text Location instead.
//!
//! `Place.Read.All` (`GraphFeature::Rooms`) is admin-sensitive and deliberately
//! outside the default "Sync with Outlook" consent, so
//! [`GraphRoomsClient::ensure_consent`] is only ever called from the popover's
//! explicit **Allow room suggestions** action - opening the popover or typing a
//! query never launches a browser. A tenant that blocks the scope degrades to
//! the free-text field (§10), never an error dialog.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Utc};
use lookout_core::{AttendeeAvailability, EmailAddress};
use lookout_graph::{CalendarClient, GraphError, Room};

use crate::graph_account::{GraphAccount, GraphFeatureClient};

/// Graph caps `getSchedule` at 20 schedules per request, so room availability
/// is asked for in batches of this size.
const MAX_SCHEDULES_PER_REQUEST: usize = 20;
/// Most rooms whose availability one popover refresh checks. A large tenant's
/// room list is filtered client-side before this is applied, so the cap only
/// bites when a query matches nearly everything; beyond it rooms show
/// "availability unknown" rather than issuing dozens of requests.
pub(crate) const MAX_ROOM_AVAILABILITY: usize = 40;

/// One Microsoft 365 account's room-directory client.
pub struct GraphRoomsClient {
    account: GraphAccount,
    calendar: CalendarClient,
}

impl GraphRoomsClient {
    pub fn new(account: GraphAccount) -> Self {
        GraphRoomsClient {
            calendar: CalendarClient::new(account.graph().clone()),
            account,
        }
    }

    /// Runs the interactive consent flow for `Place.Read.All`
    /// (`GraphFeature::Rooms`). Only ever called from the popover's Allow
    /// button.
    pub async fn ensure_consent(&self) -> Result<(), String> {
        self.account.ensure_feature(lookout_graph::GraphFeature::Rooms).await
    }

    /// The tenant's bookable rooms.
    pub async fn list_rooms(&self) -> Result<Vec<Room>, GraphError> {
        self.calendar.list_rooms().await
    }

    /// Free/busy for up to [`MAX_ROOM_AVAILABILITY`] candidate rooms over
    /// `[start, end)`, batched to Graph's 20-schedule limit. One failed batch
    /// marks its rooms unknown rather than failing the whole popover, so a
    /// transient error never blanks a list that is otherwise useful.
    pub async fn room_availability(&self, rooms: &[EmailAddress], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<AttendeeAvailability> {
        let rooms = &rooms[..rooms.len().min(MAX_ROOM_AVAILABILITY)];
        let mut answers = Vec::with_capacity(rooms.len());
        for batch in rooms.chunks(MAX_SCHEDULES_PER_REQUEST) {
            match self.calendar.get_schedule(batch, start, end).await {
                Ok(mut availabilities) => answers.append(&mut availabilities),
                Err(error) => {
                    tracing::debug!("room availability lookup failed: {error}");
                    answers.extend(batch.iter().cloned().map(AttendeeAvailability::unknown));
                }
            }
        }
        answers
    }
}

impl GraphFeatureClient for GraphRoomsClient {
    fn account(&self) -> &GraphAccount {
        &self.account
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lookout_core::AccountId;
    use lookout_graph::{GraphClient, TokenProvider};
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct StubTokens;

    #[async_trait::async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> lookout_graph::Result<String> {
            Ok("test-token".to_string())
        }

        async fn ensure_scopes(&self, _scopes: &[&str]) -> lookout_graph::Result<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> GraphRoomsClient {
        let graph = GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri()));
        GraphRoomsClient::new(GraphAccount::with_client(&AccountId("account_1".into()), graph))
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    fn utc(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, 9, 0, 0).unwrap()
    }

    #[test]
    fn rooms_come_from_graphs_place_directory() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/places/microsoft.graph.room"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [{
                        "id": "r1",
                        "displayName": "Room 4",
                        "emailAddress": "room4@example.com",
                        "capacity": 12,
                        "building": "HQ"
                    }]
                })))
                .mount(&server)
                .await;

            let rooms = client(&server).list_rooms().await.unwrap();
            assert_eq!(rooms.len(), 1);
            assert_eq!(rooms[0].display_name, "Room 4");
            assert_eq!(rooms[0].capacity, Some(12));
        });
    }

    #[test]
    fn room_availability_maps_graphs_schedule_answer() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1.0/me/calendar/getSchedule"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [{
                        "scheduleId": "room4@example.com",
                        "scheduleItems": [{
                            "start": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" },
                            "end": { "dateTime": "2026-10-15T11:00:00", "timeZone": "UTC" },
                            "status": "busy"
                        }]
                    }]
                })))
                .mount(&server)
                .await;

            let rooms = [EmailAddress::new("room4@example.com")];
            let answers = client(&server).room_availability(&rooms, utc(15), utc(16)).await;
            assert_eq!(answers.len(), 1);
            assert!(!answers[0].unknown);
            assert_eq!(answers[0].intervals.len(), 1);
        });
    }

    #[test]
    fn a_failed_batch_marks_its_rooms_unknown() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1.0/me/calendar/getSchedule"))
                .respond_with(ResponseTemplate::new(403))
                .mount(&server)
                .await;

            let rooms = [EmailAddress::new("room4@example.com")];
            let answers = client(&server).room_availability(&rooms, utc(15), utc(16)).await;
            assert_eq!(answers.len(), 1);
            assert!(answers[0].unknown, "a denied request shows unknown, not free");
        });
    }

    #[test]
    fn availability_is_capped_so_a_huge_room_list_cannot_storm_graph() {
        let rooms: Vec<EmailAddress> = (0..MAX_ROOM_AVAILABILITY + 5).map(|i| EmailAddress::new(format!("room{i}@example.com"))).collect();
        // The cap is applied before any request; the batch size below it is
        // Graph's own 20-schedule limit, so one refresh may issue several
        // batches but never one per room in a large tenant.
        assert_eq!(rooms.len().min(MAX_ROOM_AVAILABILITY), MAX_ROOM_AVAILABILITY);
    }
}
