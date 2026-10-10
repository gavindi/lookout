//! Graph permission scopes and the per-feature set each one needs
//! (OUTLOOK_PARITY.md §4.4).
//!
//! Graph accepts bare scope names (it infers the resource), so these are the
//! names as Microsoft documents them; the app prefixes them with the Graph
//! resource when building the OAuth `scope=` string. New features negotiate
//! their own scopes through [`GraphFeature::consent_scopes`], so consent is
//! asked for incrementally rather than all at once (§4.4 note).

// Copyright (C) <2026>  <Gavin Graham & Contributors>
// Software released under the GPL3 license

/// Always requested alongside a feature's own scopes: the user's own mailbox
/// read/write access that the mail-property features (categories, flags,
/// importance, Focused Inbox) share.
pub const BASE_GRAPH_SCOPES: &[&str] = &[MAIL_READ_WRITE];

pub const MAIL_READ_WRITE: &str = "Mail.ReadWrite";
pub const MAIL_READ_WRITE_SHARED: &str = "Mail.ReadWrite.Shared";
pub const MAIL_SEND: &str = "Mail.Send";
pub const MAILBOX_SETTINGS_READ_WRITE: &str = "MailboxSettings.ReadWrite";
pub const CALENDARS_READ_WRITE: &str = "Calendars.ReadWrite";
pub const CALENDARS_READ_SHARED: &str = "Calendars.Read.Shared";
pub const CALENDARS_READ_WRITE_SHARED: &str = "Calendars.ReadWrite.Shared";
pub const CONTACTS_READ_WRITE: &str = "Contacts.ReadWrite";
pub const TASKS_READ_WRITE: &str = "Tasks.ReadWrite";
pub const USER_READ_BASIC_ALL: &str = "User.ReadBasic.All";
pub const PEOPLE_READ: &str = "People.Read";
pub const PLACE_READ_ALL: &str = "Place.Read.All";

/// A Graph-backed feature and the scopes it needs (OUTLOOK_PARITY.md §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphFeature {
    /// Message categories, follow-up flags, importance, Focused Inbox and the
    /// `internetMessageId` ↔ Graph-id map. Needs [`MAIL_READ_WRITE`] (already
    /// in the base set).
    Mail,
    /// Automatic replies (Out of Office) and server-side rules.
    MailboxSettings,
    /// Server-side schedule send (`PidTagDeferredSendTime` extended property,
    /// then `send`).
    ScheduleSend,
    /// Calendar events, invitations and RSVP.
    Calendar,
    /// Free/busy lookups and meeting-time suggestions.
    FreeBusy,
    /// Room lists and rooms.
    Rooms,
    /// Another user's shared or delegated calendar.
    SharedCalendars,
    /// Contacts and contact folders.
    Contacts,
    /// The directory / GAL and people search.
    Directory,
    /// Microsoft To Do lists, tasks and checklist items.
    ToDo,
    /// A shared mailbox's folders.
    SharedMailboxes,
}

impl GraphFeature {
    /// The scopes unique to this feature (not counting [`BASE_GRAPH_SCOPES`]).
    pub fn scopes(self) -> &'static [&'static str] {
        match self {
            GraphFeature::Mail => &[MAIL_READ_WRITE],
            GraphFeature::MailboxSettings => &[MAILBOX_SETTINGS_READ_WRITE],
            GraphFeature::ScheduleSend => &[MAIL_SEND],
            GraphFeature::Calendar => &[CALENDARS_READ_WRITE],
            GraphFeature::FreeBusy => &[CALENDARS_READ_SHARED],
            GraphFeature::Rooms => &[PLACE_READ_ALL],
            GraphFeature::SharedCalendars => &[CALENDARS_READ_WRITE_SHARED],
            GraphFeature::Contacts => &[CONTACTS_READ_WRITE],
            GraphFeature::Directory => &[USER_READ_BASIC_ALL, PEOPLE_READ],
            GraphFeature::ToDo => &[TASKS_READ_WRITE],
            GraphFeature::SharedMailboxes => &[MAIL_READ_WRITE_SHARED],
        }
    }

    /// The full consent set for this feature: [`BASE_GRAPH_SCOPES`] plus its
    /// own, de-duplicated and in a stable order.
    pub fn consent_scopes(self) -> Vec<&'static str> {
        let mut scopes = BASE_GRAPH_SCOPES.to_vec();
        for scope in self.scopes() {
            if !scopes.contains(scope) {
                scopes.push(scope);
            }
        }
        scopes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_scopes_include_the_base_set_without_duplicates() {
        let mail = GraphFeature::Mail.consent_scopes();
        assert_eq!(mail, vec![MAIL_READ_WRITE], "the base scope already covers Mail");

        let calendar = GraphFeature::Calendar.consent_scopes();
        assert_eq!(calendar, vec![MAIL_READ_WRITE, CALENDARS_READ_WRITE], "base first, then the feature's own");

        let directory = GraphFeature::Directory.consent_scopes();
        assert_eq!(directory, vec![MAIL_READ_WRITE, USER_READ_BASIC_ALL, PEOPLE_READ]);
    }

    #[test]
    fn every_feature_has_at_least_one_scope() {
        let features = [
            GraphFeature::Mail,
            GraphFeature::MailboxSettings,
            GraphFeature::ScheduleSend,
            GraphFeature::Calendar,
            GraphFeature::FreeBusy,
            GraphFeature::Rooms,
            GraphFeature::SharedCalendars,
            GraphFeature::Contacts,
            GraphFeature::Directory,
            GraphFeature::ToDo,
            GraphFeature::SharedMailboxes,
        ];
        for feature in features {
            assert!(!feature.scopes().is_empty(), "{feature:?} needs a scope");
            assert!(feature.consent_scopes().contains(&BASE_GRAPH_SCOPES[0]));
        }
    }
}
