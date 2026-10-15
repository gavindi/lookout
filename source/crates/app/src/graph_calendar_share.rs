//! Sharing the user's own Microsoft 365 calendar, and opening another user's
//! (OUTLOOK_PARITY.md item 5.6 / CAL-24, CAL-25).
//!
//! Two Graph surfaces back this:
//! - `calendarPermissions` on a calendar the user owns: who it is shared with
//!   and at what role (the "Share calendar" dialog); and
//! - `/users/{id}/calendars`: another user's calendars, opened read-only or
//!   read-write depending on the grant they gave (the "Open another user's
//!   calendar" flow).
//!
//! Both need the `Calendars.ReadWrite.Shared` scope
//! (`GraphFeature::SharedCalendars`), which is admin-sensitive and so requested
//! per-feature rather than in the default combined consent. Every call is
//! gated by [`GraphCalendarShareClient::consented`], mirroring the other
//! app-side Graph clients.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;

use lookout_core::{AccountId, CalendarInfo};
use lookout_graph::{CalendarClient, CalendarPermission, CalendarRole, GraphClient, GraphError, GraphUser};

use crate::microsoft_oauth::MicrosoftOAuth;

/// One Microsoft 365 account's calendar-sharing client.
pub struct GraphCalendarShareClient {
    oauth: Arc<MicrosoftOAuth>,
    graph: GraphClient,
}

impl GraphCalendarShareClient {
    pub fn new(account_id: AccountId) -> Self {
        let oauth = Arc::new(MicrosoftOAuth::new(account_id));
        GraphCalendarShareClient {
            graph: GraphClient::new(oauth.clone()),
            oauth,
        }
    }

    /// Whether the account has granted Graph access (disk-only check).
    pub fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Runs the interactive consent flow for the shared-calendar scope
    /// (`Calendars.ReadWrite.Shared`). Only ever called from a user action.
    pub async fn ensure_consent(&self) -> Result<(), String> {
        self.graph.ensure_feature(lookout_graph::GraphFeature::SharedCalendars).await.map_err(|e| e.to_string())
    }

    /// Resolves another user's address to a directory user, so their calendars
    /// can be opened.
    pub async fn resolve_user(&self, address: &str) -> Result<Option<GraphUser>, GraphError> {
        lookout_graph::resolve_user(&self.graph, address).await
    }

    /// Another user's calendars (`/users/{owner}/calendars`).
    pub async fn list_calendars(&self, account_id: &AccountId, owner: &str) -> Result<Vec<CalendarInfo>, GraphError> {
        CalendarClient::new(GraphClient::new(self.oauth.clone())).list_calendars(account_id, Some(owner)).await
    }

    /// The grants on `calendar` (the user's own).
    pub async fn list_permissions(&self, calendar: &CalendarInfo) -> Result<Vec<CalendarPermission>, GraphError> {
        CalendarClient::new(GraphClient::new(self.oauth.clone())).list_permissions(calendar).await
    }

    /// Grants `email` the given `role` on `calendar`.
    pub async fn create_permission(&self, calendar: &CalendarInfo, email: &str, role: CalendarRole, send_notification: bool) -> Result<CalendarPermission, GraphError> {
        CalendarClient::new(GraphClient::new(self.oauth.clone()))
            .create_permission(calendar, email, role, send_notification)
            .await
    }

    /// Revokes a grant by its id.
    pub async fn delete_permission(&self, calendar: &CalendarInfo, permission_id: &str) -> Result<(), GraphError> {
        CalendarClient::new(GraphClient::new(self.oauth.clone())).delete_permission(calendar, permission_id).await
    }
}

/// The share dialog's role dropdown, in Outlook's order (least to most access).
/// The label is Outlook's phrasing; the value is the Graph role.
pub const SHARE_ROLES: &[(CalendarRole, &str)] = &[
    (CalendarRole::FreeBusyRead, "Can view when I'm busy"),
    (CalendarRole::LimitedRead, "Can view titles and locations"),
    (CalendarRole::Read, "Can view all details"),
    (CalendarRole::Write, "Can edit"),
];

/// The Outlook label for a granted role, for the permission rows.
pub fn role_label(role: CalendarRole) -> &'static str {
    SHARE_ROLES.iter().find(|(r, _)| *r == role).map(|(_, label)| *label).unwrap_or("Can view when I'm busy")
}

/// The role for a dropdown index in [`SHARE_ROLES`].
pub fn role_for_index(index: u32) -> CalendarRole {
    SHARE_ROLES.get(index as usize).map(|(role, _)| *role).unwrap_or(CalendarRole::FreeBusyRead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_labels_round_trip_through_the_dropdown() {
        for (index, (role, label)) in SHARE_ROLES.iter().enumerate() {
            assert_eq!(role_for_index(index as u32), *role);
            assert_eq!(role_label(*role), *label);
        }
    }

    #[test]
    fn an_out_of_range_index_degrades_to_free_busy_read() {
        assert_eq!(role_for_index(99), CalendarRole::FreeBusyRead);
    }

    #[test]
    fn the_dropdown_is_least_to_most_access() {
        assert_eq!(SHARE_ROLES[0].0, CalendarRole::FreeBusyRead);
        assert_eq!(SHARE_ROLES[SHARE_ROLES.len() - 1].0, CalendarRole::Write);
    }
}
