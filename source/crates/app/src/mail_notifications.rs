//! Desktop notifications for mail: new-mail arrivals and send failures.
//!
//! Built on the same `gio::Notification` + `Application::send_notification`
//! mechanism as `reminders.rs` for calendar alerts - not GNOME-specific, it
//! renders through whatever freedesktop-spec notification daemon the desktop
//! runs (GNOME Shell, KDE Plasma, XFCE's xfce4-notifyd, dunst, mako, ...).
//! The window code decides *whether* a given event is worth notifying
//! (the `MAIL_NOTIFICATIONS_ENABLED` setting, the mailbox's role, whether the
//! window is already focused on it) and only calls into this module once it
//! has - everything here just builds and sends.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::rc::Rc;

use gtk::gio;
use gtk::gio::prelude::{ActionMapExt, ApplicationExt};
use gtk::glib;
use lookout_core::{AccountId, EmailSummary, MailboxId, MailboxRole};

/// Whether new mail in a mailbox with this role is worth a desktop
/// notification. Only user-facing mail folders qualify - Sent, Drafts,
/// Trash, Junk and Archive are populated by the app's own actions (or hold
/// mail nobody is waiting to be told about), not things "arriving" for the
/// user to read.
pub fn should_notify_role(role: MailboxRole) -> bool {
    matches!(role, MailboxRole::Inbox | MailboxRole::Custom)
}

/// The notification id for new mail in `mailbox`: stable per mailbox, so a
/// burst of arrivals (several IDLE wakes in a row) replaces the previous
/// bubble instead of stacking duplicates.
fn new_mail_notification_id(mailbox: &MailboxId) -> String {
    format!("new-mail:{}", mailbox.0)
}

/// The notification id for a send failure on `account_id`: stable per
/// account, so a reconnect-retry storm replaces rather than spams.
fn send_failed_notification_id(account_id: &AccountId) -> String {
    format!("send-failed:{}", account_id.0)
}

/// The title/body for a new-mail notification. A single message names its
/// sender and subject; more than one is summarized by count and folder,
/// since listing every subject would make for an unreadable bubble.
fn new_mail_text(mailbox_label: &str, messages: &[EmailSummary]) -> (String, String) {
    if let [only] = messages {
        let from = only.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "New message".to_string());
        let subject = only.subject.clone().unwrap_or_else(|| "(No subject)".to_string());
        (from, subject)
    } else {
        (format!("{} new messages", messages.len()), format!("in {mailbox_label}"))
    }
}

/// Builds and sends a "new mail" notification for `messages` (already
/// filtered by the caller to genuinely-new, unread, notification-worthy
/// messages). The default action fires `app.open-mailbox` targeted at
/// `mailbox`, handled by whatever closure `spawn_actions` registered.
pub fn show_new_mail_notification(app: &adw::Application, mailbox_label: &str, mailbox: &MailboxId, messages: &[EmailSummary]) {
    let (title, body) = new_mail_text(mailbox_label, messages);
    let notification = gio::Notification::new(&title);
    notification.set_body(Some(&body));
    let variant = glib::Variant::from(mailbox.0.clone());
    notification.set_default_action_and_target_value("app.open-mailbox", Some(&variant));
    app.send_notification(Some(&new_mail_notification_id(mailbox)), &notification);
}

/// Withdraws `mailbox`'s outstanding "new mail" notification, if any -
/// called once its unread count is back to 0 so the notification doesn't
/// linger and keep some desktop shells' per-app notification-count badge
/// (a different mechanism from the `LauncherEntry` count in
/// `launcher_entry.rs`) stuck showing it as unread forever. Withdrawing an
/// id with nothing outstanding is a harmless no-op.
pub fn withdraw_new_mail_notification(app: &adw::Application, mailbox: &MailboxId) {
    app.withdraw_notification(&new_mail_notification_id(mailbox));
}

/// Builds and sends a "message not sent" notification. The default action
/// just raises the window (`app.raise-window`) - there's no specific place
/// to navigate to for a send failure.
pub fn show_send_failed_notification(app: &adw::Application, account_id: &AccountId, message: &str) {
    let notification = gio::Notification::new("Message not sent");
    notification.set_body(Some(message));
    notification.set_default_action("app.raise-window");
    app.send_notification(Some(&send_failed_notification_id(account_id)), &notification);
}

/// Registers the `raise-window` and `open-mailbox` actions the notifications
/// above target when clicked. `open_mailbox` and `raise_window` are handed in
/// from the window code, which owns the actual navigation - mirrors
/// `reminders::spawn_reminder_loop`'s `open_event` closure.
pub fn spawn_actions(app: &adw::Application, open_mailbox: Rc<dyn Fn(MailboxId)>, raise_window: Rc<dyn Fn()>) {
    let raise_action = gio::SimpleAction::new("raise-window", None);
    raise_action.connect_activate(move |_, _| raise_window());
    app.add_action(&raise_action);

    let open_action = gio::SimpleAction::new("open-mailbox", Some(glib::VariantTy::STRING));
    open_action.connect_activate(move |_, param| {
        let Some(id) = param.and_then(|v| v.get::<String>()) else { return };
        open_mailbox(MailboxId(id));
    });
    app.add_action(&open_action);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::EmailAddress;

    fn summary(subject: Option<&str>, from: Option<&str>) -> EmailSummary {
        EmailSummary {
            uid: lookout_core::Uid(1),
            mailbox: MailboxId("acc:INBOX".into()),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: subject.map(str::to_string),
            from: from.map(|f| vec![EmailAddress::new(f)]).unwrap_or_default(),
            to: Vec::new(),
            cc: Vec::new(),
            date: chrono::Utc::now(),
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            structure: None,
        }
    }

    #[test]
    fn should_notify_role_covers_inbox_and_custom_only() {
        assert!(should_notify_role(MailboxRole::Inbox));
        assert!(should_notify_role(MailboxRole::Custom));
        assert!(!should_notify_role(MailboxRole::Sent));
        assert!(!should_notify_role(MailboxRole::Drafts));
        assert!(!should_notify_role(MailboxRole::Trash));
        assert!(!should_notify_role(MailboxRole::Junk));
        assert!(!should_notify_role(MailboxRole::Archive));
    }

    #[test]
    fn single_message_names_sender_and_subject() {
        let (title, body) = new_mail_text("Inbox", &[summary(Some("Hello"), Some("alice@example.com"))]);
        assert_eq!(title, "alice@example.com");
        assert_eq!(body, "Hello");
    }

    #[test]
    fn single_message_falls_back_when_sender_or_subject_are_missing() {
        let (title, body) = new_mail_text("Inbox", &[summary(None, None)]);
        assert_eq!(title, "New message");
        assert_eq!(body, "(No subject)");
    }

    #[test]
    fn multiple_messages_are_summarized_by_count_and_folder() {
        let messages = vec![summary(Some("A"), Some("a@example.com")), summary(Some("B"), Some("b@example.com"))];
        let (title, body) = new_mail_text("Work", &messages);
        assert_eq!(title, "2 new messages");
        assert_eq!(body, "in Work");
    }

    #[test]
    fn notification_ids_are_stable_per_mailbox_and_account() {
        let mailbox = MailboxId("acc:INBOX".into());
        assert_eq!(new_mail_notification_id(&mailbox), new_mail_notification_id(&mailbox));
        let account = AccountId("acc".into());
        assert_eq!(send_failed_notification_id(&account), send_failed_notification_id(&account));
    }
}
