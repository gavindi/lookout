//! The message context menu (OUTLOOK_PARITY.md item 1.17 / M-38, UX-36): what
//! a right-click on a message row (or a conversation header) offers, in new
//! Outlook's order, built on the shared [`crate::context_menu`] builder.
//!
//! This module is the pure half: which entries the menu shows for the
//! messages it acts on ([`message_menu_state`]), the [`MenuDescription`] for
//! that state, and the mapping from an activated item back to a
//! [`MessageMenuAction`]. `window.rs` owns the effects - every action runs
//! through the same helpers as the ribbon, so each verb behaves (and undoes)
//! the same wherever it's invoked.
//!
//! Entries whose features haven't landed yet are left out rather than shown
//! greyed. Ignore / Clean up conversation (2.8) are included. Report ▸ Report
//! phishing (6.7) is included only when a reporting address is set; Block ▸
//! (2.9) offers Block sender / Block domain, or Unblock sender when the sender
//! is already blocked. Each adds itself here when it lands.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use chrono::{DateTime, NaiveDate, Utc};
use gtk::glib;
use gtk::prelude::*;
use lookout_core::{FollowUpFlag, Importance, MailboxId, MailboxRole};

use crate::context_menu::{ActionSpec, MenuDescription};

/// The action namespace the menu's group is inserted under.
pub const NAMESPACE: &str = "msg";

/// The custom-widget slot holding the Categorize checklist.
pub const CATEGORIZE_SLOT: &str = "categorize";

/// How many recently used folders Move ▸ / Copy ▸ list above "Other folder…".
pub const RECENT_FOLDER_LIMIT: usize = 5;

/// What a menu item asks for. The targets are fixed when the menu opens.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageMenuAction {
    Reply,
    ReplyAll,
    Forward,
    /// Open a new calendar event prefilled with the conversation's participants
    /// and subject (item 3.19 / C-20): Outlook's "Reply with meeting".
    ReplyWithMeeting,
    Delete,
    Archive,
    MoveTo(MailboxId),
    MoveOther,
    CopyTo(MailboxId),
    CopyOther,
    /// Mark as read (`true`) or unread.
    SetRead(bool),
    Flag(FollowUpFlag),
    FlagCustom,
    /// Pin (`true`) or Unpin.
    Pin(bool),
    Snooze(DateTime<Utc>),
    SnoozeCustom,
    Unsnooze,
    ReportJunk,
    /// Mark the targets as not junk (move back to the Inbox and set RFC 5788
    /// `$NotJunk`, item 1.24); offered inside the Junk folder.
    NotJunk,
    /// Forward the target as an attachment to the configured reporting
    /// address, then move it to Junk (item 6.7 / S-09). Offered only when a
    /// report-phishing address is set.
    ReportPhishing,
    /// Create a rule prefilled from the target message (item 2.5 / M-14).
    CreateRule,
    /// Block the target's sender (item 2.9 / M-17): add the exact address to
    /// the account's blocked list, then move the target to Junk.
    BlockSender,
    /// Block every address on the target sender's domain, then move the target
    /// to Junk.
    BlockDomain,
    /// Remove the target's sender from the account's blocked list.
    UnblockSender,
    /// Ignore the target conversation (item 2.8 / M-20): remember its thread
    /// key so future arrivals move to Deleted, and move the conversation's
    /// messages there now.
    Ignore,
    /// Stop ignoring the target conversation (remove the thread key).
    StopIgnoring,
    /// Delete the conversation's redundant, wholly-quoted messages (item 2.8 /
    /// M-20).
    CleanUpConversation,
    /// Reclassify the targets' Focused/Other bucket (M-03): `true` = Focused.
    MoveFocused(bool),
    /// Also record a sender override so future mail follows: `true` = Focused
    /// ("Always move…").
    AlwaysMoveFocused(bool),
    /// Delete the target sender's Focused/Other override ("Stop moving…").
    StopMovingFocused,
    /// Set the targets' importance (M-12).
    SetImportance(Importance),
    ViewDetails,
    ViewSource,
    OpenWindow,
    Print,
}

/// Which entries the menu offers, and how each reads, for the messages it
/// acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageMenuState {
    /// There's one message for the single-message verbs (Reply, Forward,
    /// View ▸, Open in new window, Print): a lone target, or a
    /// conversation's newest message.
    pub single: bool,
    /// Reply / Reply all / Forward: a single message that isn't a draft.
    pub can_reply: bool,
    /// Block ▸ (item 2.9 / M-17): a single message that isn't a draft (there
    /// is no sender to block for a draft).
    pub can_block: bool,
    /// The targets' account has an Archive folder and they aren't in it.
    pub can_archive: bool,
    /// Move ▸ / Copy ▸: every target lives in one account (folders can't
    /// span accounts).
    pub can_move: bool,
    /// "Mark as read" when any target is unread, else "Mark as unread"
    /// (UX-07: the label names what will happen).
    pub any_unread: bool,
    /// "Unpin" only when every target is pinned, as the ribbon's Pin.
    pub all_pinned: bool,
    /// Pin has no Graph counterpart (5.7.7), so the entry is left out for
    /// every target on the Graph mail transport - the module's "features not
    /// available are left out" rule (degrade softly, never an error).
    pub can_pin: bool,
    /// Snooze ▸ (not for drafts).
    pub can_snooze: bool,
    /// Some target is snoozed (waiting in Scheduled): Unsnooze replaces
    /// Snooze ▸.
    pub snoozed: bool,
    /// Report ▸ Report junk (not from the Junk folder itself).
    pub can_report_junk: bool,
    /// Report ▸ Not junk (every target is in the Junk folder).
    pub can_not_junk: bool,
    /// Focused Inbox ▸ (M-03): every target lives in one account.
    pub can_focused: bool,
}

/// The facts [`message_menu_state`] needs about one target message.
#[derive(Debug, Clone)]
pub struct TargetFacts {
    pub account: String,
    pub role: Option<MailboxRole>,
    pub unread: bool,
    pub pinned: bool,
    pub draft: bool,
    pub snoozed: bool,
    /// This message is on the Graph mail transport, where Pin is unavailable
    /// (item 5.7.7).
    pub graph_mail: bool,
}

/// The menu's state for `targets`. `single` says whether the single-message
/// verbs have a message to act on; `has_archive` whether the targets'
/// account has an Archive folder.
pub fn message_menu_state(targets: &[TargetFacts], single: bool, has_archive: bool) -> MessageMenuState {
    if targets.is_empty() {
        return MessageMenuState::default();
    }
    let any_draft = targets.iter().any(|t| t.draft || t.role == Some(MailboxRole::Drafts));
    let one_account = targets.iter().all(|t| t.account == targets[0].account);
    MessageMenuState {
        single,
        can_reply: single && !any_draft,
        can_block: single && !any_draft,
        can_archive: one_account && has_archive && targets.iter().all(|t| t.role != Some(MailboxRole::Archive)),
        can_move: one_account,
        any_unread: targets.iter().any(|t| t.unread),
        all_pinned: targets.iter().all(|t| t.pinned),
        can_pin: targets.iter().all(|t| !t.graph_mail),
        can_snooze: !any_draft,
        snoozed: targets.iter().any(|t| t.snoozed),
        can_report_junk: targets.iter().all(|t| t.role != Some(MailboxRole::Junk)),
        can_not_junk: targets.iter().all(|t| t.role == Some(MailboxRole::Junk)),
        can_focused: one_account,
    }
}

/// One Snooze ▸ entry: its label ("Tomorrow (Wed 08:00)") and wake time.
#[derive(Debug, Clone, PartialEq)]
pub struct SnoozeEntry {
    pub label: String,
    pub at: DateTime<Utc>,
}

/// One Flag ▸ entry: its name and due date (`None` = "No date").
#[derive(Debug, Clone, PartialEq)]
pub struct FlagEntry {
    pub name: String,
    pub due: Option<NaiveDate>,
}

/// Everything besides the state the description is built from: Move ▸ /
/// Copy ▸'s recent folders (id, display name), and the Flag ▸ / Snooze ▸
/// presets for the moment the menu opens.
#[derive(Debug, Clone, Default)]
pub struct MenuChoices {
    pub recent_folders: Vec<(MailboxId, String)>,
    pub flags: Vec<FlagEntry>,
    pub snoozes: Vec<SnoozeEntry>,
    /// Whether a report-phishing address is configured (item 6.7 / S-09).
    /// When false the Report ▸ Report phishing entry is left out entirely, as
    /// the module docs' "features not landed yet" rule does elsewhere.
    pub report_phishing: bool,
    /// Whether the target sender is already on the account's blocked list
    /// (item 2.9 / M-17): when true the Block ▸ submenu offers Unblock sender
    /// instead of Block sender / Block domain.
    pub sender_blocked: bool,
    /// Whether the target account provides Focused Inbox (M-03 / item 5.4):
    /// when true the Focused Inbox ▸ submenu is offered. Left out entirely
    /// otherwise, per the module docs' "features not available are left out"
    /// rule.
    pub focused_inbox: bool,
    /// Whether the target conversation is already ignored (item 2.8 / M-20):
    /// when true the menu offers "Stop ignoring" instead of "Ignore".
    pub thread_ignored: bool,
}

/// Every action the menu's group carries.
pub fn action_specs() -> Vec<ActionSpec> {
    let mut specs: Vec<ActionSpec> = [
        "reply",
        "reply-all",
        "forward",
        "reply-with-meeting",
        "delete",
        "archive",
        "move-other",
        "copy-other",
        "mark-read",
        "mark-unread",
        "flag-custom",
        "flag-complete",
        "flag-clear",
        "pin",
        "unpin",
        "snooze-custom",
        "unsnooze",
        "report-junk",
        "not-junk",
        "report-phishing",
        "create-rule",
        "block-sender",
        "block-domain",
        "unblock-sender",
        "ignore",
        "stop-ignoring",
        "clean-up-conversation",
        "importance-high",
        "importance-normal",
        "importance-low",
        "focus-focused",
        "focus-other",
        "focus-always-focused",
        "focus-always-other",
        "focus-stop",
        "view-details",
        "view-source",
        "open-window",
        "print",
    ]
    .into_iter()
    .map(ActionSpec::simple)
    .collect();
    specs.push(ActionSpec::parameterised("move", glib::VariantTy::STRING));
    specs.push(ActionSpec::parameterised("copy", glib::VariantTy::STRING));
    specs.push(ActionSpec::parameterised("flag", glib::VariantTy::STRING));
    specs.push(ActionSpec::parameterised("snooze", glib::VariantTy::INT64));
    specs
}

/// The bare names of the actions the menu greys out by state, with whether
/// each is enabled in `state`.
pub fn enabled_actions(state: &MessageMenuState) -> Vec<(&'static str, bool)> {
    vec![
        ("reply", state.can_reply),
        ("reply-all", state.can_reply),
        ("forward", state.can_reply),
        ("reply-with-meeting", state.can_reply),
        ("archive", state.can_archive),
        ("move", state.can_move),
        ("move-other", state.can_move),
        ("copy", state.can_move),
        ("copy-other", state.can_move),
        ("snooze", state.can_snooze),
        ("snooze-custom", state.can_snooze),
        ("report-junk", state.can_report_junk),
        ("not-junk", state.can_not_junk),
        ("block-sender", state.can_block),
        ("block-domain", state.can_block),
        ("unblock-sender", state.can_block),
        ("importance-high", state.can_focused),
        ("importance-normal", state.can_focused),
        ("importance-low", state.can_focused),
        ("focus-focused", state.can_focused),
        ("focus-other", state.can_focused),
        ("focus-always-focused", state.single),
        ("focus-always-other", state.single),
        ("focus-stop", state.single),
        ("create-rule", state.single),
        ("view-details", state.single),
        ("view-source", state.single),
        ("open-window", state.single),
        ("print", state.single),
    ]
}

fn ns(name: &str) -> String {
    format!("{NAMESPACE}.{name}")
}

/// A folder submenu: the recent folders, then "Other folder…".
fn folder_submenu(action: &str, other: &str, recent: &[(MailboxId, String)]) -> MenuDescription {
    let mut menu = MenuDescription::new().section();
    for (id, name) in recent {
        menu = menu.action_with_target(name.clone(), ns(action), id.0.to_variant());
    }
    menu.section().icon_action("Other folder…", ns(other), "folder-symbolic")
}

/// The menu for `state`, in Outlook's order (UX-36), less the entries still
/// to come (see the module docs).
pub fn message_menu_description(state: &MessageMenuState, choices: &MenuChoices) -> MenuDescription {
    let mut flag = MenuDescription::new().section();
    for entry in &choices.flags {
        let target = entry.due.map(|due| due.format("%Y-%m-%d").to_string()).unwrap_or_default();
        flag = flag.action_with_target(entry.name.clone(), ns("flag"), target.to_variant());
    }
    let flag = flag
        .icon_action("Custom date…", ns("flag-custom"), "mail-flag-symbolic")
        .section()
        .icon_action("Mark complete", ns("flag-complete"), "object-select-symbolic")
        .icon_action("Clear flag", ns("flag-clear"), "edit-clear-symbolic");

    let mut menu = MenuDescription::new()
        .section()
        .icon_action("Reply", ns("reply"), "mail-reply-sender-symbolic")
        .icon_action("Reply all", ns("reply-all"), "mail-reply-all-symbolic")
        .icon_action("Forward", ns("forward"), "mail-forward-symbolic")
        .icon_action("Reply with meeting", ns("reply-with-meeting"), "x-office-calendar-symbolic")
        .section()
        .icon_action("Delete", ns("delete"), "user-trash-symbolic")
        .icon_action("Archive", ns("archive"), "mail-archive-symbolic")
        .submenu("Move", folder_submenu("move", "move-other", &choices.recent_folders))
        .submenu("Copy", folder_submenu("copy", "copy-other", &choices.recent_folders))
        .section();
    menu = if state.any_unread {
        menu.icon_action("Mark as read", ns("mark-read"), "object-select-symbolic")
    } else {
        menu.icon_action("Mark as unread", ns("mark-unread"), "mail-unread-symbolic")
    };
    menu = menu.submenu("Flag", flag);
    // Pin is left out on the Graph mail transport, which has no counterpart
    // for it (item 5.7.7).
    if state.can_pin {
        menu = if state.all_pinned {
            menu.icon_action("Unpin", ns("unpin"), "view-pin-symbolic")
        } else {
            menu.icon_action("Pin", ns("pin"), "view-pin-symbolic")
        };
    }
    menu = if state.snoozed {
        menu.icon_action("Unsnooze", ns("unsnooze"), "alarm-symbolic")
    } else {
        let mut snooze = MenuDescription::new().section();
        for entry in &choices.snoozes {
            snooze = snooze.action_with_target(entry.label.clone(), ns("snooze"), entry.at.timestamp().to_variant());
        }
        menu.submenu("Snooze", snooze.section().icon_action("Choose a date…", ns("snooze-custom"), "appointment-soon-symbolic"))
    };
    menu = menu.submenu("Categorize", MenuDescription::new().section().custom(CATEGORIZE_SLOT));
    // Tags group (M-12, M-03): Importance ▸ is universal; Focused Inbox ▸ is
    // offered on an account that provides a classification - Graph on
    // Microsoft 365, the local classifier elsewhere (items 5.4, 2.12).
    menu = menu.submenu(
        "Importance",
        MenuDescription::new()
            .section()
            .icon_action("High", ns("importance-high"), "mail-mark-important-symbolic")
            .icon_action("Normal", ns("importance-normal"), "mail-mark-important-symbolic")
            .icon_action("Low", ns("importance-low"), "mail-mark-important-symbolic"),
    );
    if choices.focused_inbox {
        menu = menu.submenu(
            "Focused Inbox",
            MenuDescription::new()
                .section()
                .icon_action("Move to Focused", ns("focus-focused"), "view-list-symbolic")
                .icon_action("Move to Other", ns("focus-other"), "view-list-symbolic")
                .section()
                .icon_action("Always move to Focused", ns("focus-always-focused"), "view-list-symbolic")
                .icon_action("Always move to Other", ns("focus-always-other"), "view-list-symbolic")
                .icon_action("Stop moving from this sender", ns("focus-stop"), "edit-clear-symbolic"),
        );
    }
    menu = menu.section();
    // Ignore / Clean up (item 2.8 / M-20) precede Report ▸ in Outlook's order
    // (UX-36). "Stop ignoring" replaces "Ignore" for an already-ignored thread.
    menu = if choices.thread_ignored {
        menu.icon_action("Stop ignoring", ns("stop-ignoring"), "edit-clear-symbolic")
    } else {
        menu.icon_action("Ignore", ns("ignore"), "action-unavailable-symbolic")
    };
    menu = menu.icon_action("Clean up conversation", ns("clean-up-conversation"), "edit-clear-all-symbolic");
    let mut report = MenuDescription::new().section().icon_action("Report junk", ns("report-junk"), "mail-mark-junk-symbolic");
    if choices.report_phishing {
        report = report.icon_action("Report phishing", ns("report-phishing"), "dialog-warning-symbolic");
    }
    report = report.icon_action("Not junk", ns("not-junk"), "mail-mark-notjunk-symbolic");
    menu = menu.submenu("Report", report).section();
    // Block ▸ (item 2.9 / M-17): add the sender (or its whole domain) to the
    // account's blocked list, or - when the sender is already blocked - remove
    // it. Sits between Report ▸ and Rules ▸ in Outlook's order (UX-36).
    let block = if choices.sender_blocked {
        MenuDescription::new()
            .section()
            .icon_action("Unblock sender", ns("unblock-sender"), "object-select-symbolic")
    } else {
        MenuDescription::new()
            .section()
            .icon_action("Block sender", ns("block-sender"), "action-unavailable-symbolic")
            .icon_action("Block domain", ns("block-domain"), "action-unavailable-symbolic")
    };
    menu = menu.submenu("Block", block).section();
    menu = menu
        .submenu(
            "Rules",
            MenuDescription::new().section().icon_action("Create rule…", ns("create-rule"), "emblem-system-symbolic"),
        )
        .section();
    menu.submenu(
        "View",
        MenuDescription::new()
            .section()
            .icon_action("View message details", ns("view-details"), "document-properties-symbolic")
            .icon_action("View message source", ns("view-source"), "text-x-generic-symbolic"),
    )
    .section()
    .icon_action("Open in new window", ns("open-window"), "window-new-symbolic")
    .icon_action("Print", ns("print"), "printer-symbolic")
}

/// Maps an activated action's bare name and parameter back to what it asks
/// for. `None` for an unknown name or a malformed parameter.
pub fn message_action_for(name: &str, param: Option<&glib::Variant>) -> Option<MessageMenuAction> {
    let string = || param.and_then(|p| p.get::<String>());
    Some(match name {
        "reply" => MessageMenuAction::Reply,
        "reply-all" => MessageMenuAction::ReplyAll,
        "forward" => MessageMenuAction::Forward,
        "reply-with-meeting" => MessageMenuAction::ReplyWithMeeting,
        "delete" => MessageMenuAction::Delete,
        "archive" => MessageMenuAction::Archive,
        "move" => MessageMenuAction::MoveTo(MailboxId(string().filter(|id| !id.is_empty())?)),
        "move-other" => MessageMenuAction::MoveOther,
        "copy" => MessageMenuAction::CopyTo(MailboxId(string().filter(|id| !id.is_empty())?)),
        "copy-other" => MessageMenuAction::CopyOther,
        "mark-read" => MessageMenuAction::SetRead(true),
        "mark-unread" => MessageMenuAction::SetRead(false),
        "flag" => {
            let text = string()?;
            let due = if text.is_empty() {
                None
            } else {
                Some(NaiveDate::parse_from_str(&text, "%Y-%m-%d").ok()?)
            };
            MessageMenuAction::Flag(crate::follow_up::flagged(due))
        }
        "flag-custom" => MessageMenuAction::FlagCustom,
        "flag-complete" => MessageMenuAction::Flag(crate::follow_up::completed()),
        "flag-clear" => MessageMenuAction::Flag(crate::follow_up::cleared()),
        "pin" => MessageMenuAction::Pin(true),
        "unpin" => MessageMenuAction::Pin(false),
        "snooze" => MessageMenuAction::Snooze(DateTime::from_timestamp(param?.get::<i64>()?, 0)?),
        "snooze-custom" => MessageMenuAction::SnoozeCustom,
        "unsnooze" => MessageMenuAction::Unsnooze,
        "report-junk" => MessageMenuAction::ReportJunk,
        "not-junk" => MessageMenuAction::NotJunk,
        "report-phishing" => MessageMenuAction::ReportPhishing,
        "block-sender" => MessageMenuAction::BlockSender,
        "block-domain" => MessageMenuAction::BlockDomain,
        "unblock-sender" => MessageMenuAction::UnblockSender,
        "ignore" => MessageMenuAction::Ignore,
        "stop-ignoring" => MessageMenuAction::StopIgnoring,
        "clean-up-conversation" => MessageMenuAction::CleanUpConversation,
        "importance-high" => MessageMenuAction::SetImportance(Importance::High),
        "importance-normal" => MessageMenuAction::SetImportance(Importance::Normal),
        "importance-low" => MessageMenuAction::SetImportance(Importance::Low),
        "focus-focused" => MessageMenuAction::MoveFocused(true),
        "focus-other" => MessageMenuAction::MoveFocused(false),
        "focus-always-focused" => MessageMenuAction::AlwaysMoveFocused(true),
        "focus-always-other" => MessageMenuAction::AlwaysMoveFocused(false),
        "focus-stop" => MessageMenuAction::StopMovingFocused,
        "create-rule" => MessageMenuAction::CreateRule,
        "view-details" => MessageMenuAction::ViewDetails,
        "view-source" => MessageMenuAction::ViewSource,
        "open-window" => MessageMenuAction::OpenWindow,
        "print" => MessageMenuAction::Print,
        _ => return None,
    })
}

/// Puts `folder` at the front of an account's recent-folder list, dropping
/// any earlier mention and keeping at most [`RECENT_FOLDER_LIMIT`].
pub fn push_recent_folder(recent: &mut Vec<MailboxId>, folder: MailboxId) {
    recent.retain(|id| *id != folder);
    recent.insert(0, folder);
    recent.truncate(RECENT_FOLDER_LIMIT);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_menu::MenuItem;

    fn facts(role: Option<MailboxRole>) -> TargetFacts {
        TargetFacts {
            account: "acc".into(),
            role,
            unread: false,
            pinned: false,
            draft: false,
            snoozed: false,
            graph_mail: false,
        }
    }

    /// The labels of a description, section by section, with submenus as
    /// "Label ▸".
    fn labels(menu: &MenuDescription) -> Vec<Vec<String>> {
        menu.sections_for_test()
            .iter()
            .map(|section| {
                section
                    .iter()
                    .map(|item| match item {
                        MenuItem::Action(action) => action.label.clone(),
                        MenuItem::Submenu { label, .. } => format!("{label} ▸"),
                        MenuItem::Custom(id) => format!("[{id}]"),
                    })
                    .collect()
            })
            .collect()
    }

    fn submenu<'a>(menu: &'a MenuDescription, name: &str) -> &'a MenuDescription {
        menu.sections_for_test()
            .iter()
            .flatten()
            .find_map(|item| match item {
                MenuItem::Submenu { label, menu } if label == name => Some(menu),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {name} submenu"))
    }

    #[test]
    fn the_menu_follows_outlooks_order() {
        let state = message_menu_state(&[facts(Some(MailboxRole::Inbox))], true, true);
        let menu = message_menu_description(&state, &MenuChoices::default());
        assert_eq!(
            labels(&menu),
            vec![
                vec!["Reply", "Reply all", "Forward", "Reply with meeting"],
                vec!["Delete", "Archive", "Move ▸", "Copy ▸"],
                vec!["Mark as unread", "Flag ▸", "Pin", "Snooze ▸", "Categorize ▸", "Importance ▸"],
                vec!["Ignore", "Clean up conversation", "Report ▸"],
                vec!["Block ▸"],
                vec!["Rules ▸"],
                vec!["View ▸"],
                vec!["Open in new window", "Print"],
            ]
        );
        assert_eq!(labels(submenu(&menu, "Report")), vec![vec!["Report junk", "Not junk"]]);
        // Block ▸ offers Block sender / Block domain until the sender is
        // blocked, when it becomes Unblock sender (item 2.9 / M-17).
        assert_eq!(labels(submenu(&menu, "Block")), vec![vec!["Block sender", "Block domain"]]);
        let blocked = MenuChoices {
            sender_blocked: true,
            ..MenuChoices::default()
        };
        let blocked_menu = message_menu_description(&state, &blocked);
        assert_eq!(labels(submenu(&blocked_menu, "Block")), vec![vec!["Unblock sender"]]);
        // Report phishing joins the Report menu only when an address is set.
        let configured = MenuChoices {
            report_phishing: true,
            ..MenuChoices::default()
        };
        let menu = message_menu_description(&state, &configured);
        assert_eq!(labels(submenu(&menu, "Report")), vec![vec!["Report junk", "Report phishing", "Not junk"]]);
        assert_eq!(labels(submenu(&menu, "View")), vec![vec!["View message details", "View message source"]]);
        assert_eq!(labels(submenu(&menu, "Categorize")), vec![vec![format!("[{CATEGORIZE_SLOT}]")]]);
        // Focused Inbox ▸ appears only on an account that provides it (M-03).
        let focused = MenuChoices {
            focused_inbox: true,
            ..MenuChoices::default()
        };
        let focused_menu = message_menu_description(&state, &focused);
        assert_eq!(
            labels(submenu(&focused_menu, "Focused Inbox")),
            vec![
                vec!["Move to Focused", "Move to Other"],
                vec!["Always move to Focused", "Always move to Other", "Stop moving from this sender"]
            ]
        );
        assert_eq!(labels(submenu(&focused_menu, "Importance")), vec![vec!["High", "Normal", "Low"]]);
    }

    #[test]
    fn graph_mail_leaves_pin_out() {
        // The Graph mail transport has no Pin counterpart (item 5.7.7), so
        // the entry is absent rather than a no-op or an error.
        let mut target = facts(Some(MailboxRole::Inbox));
        target.graph_mail = true;
        let state = message_menu_state(&[target], true, true);
        assert!(!state.can_pin);
        let menu = message_menu_description(&state, &MenuChoices::default());
        let shown = labels(&menu);
        assert!(
            !shown.iter().flatten().any(|label| label == "Pin" || label == "Unpin"),
            "Pin must be left out on Graph mail: {shown:?}"
        );
        // Everything else the transport supports stays in place.
        assert!(shown.iter().flatten().any(|label| label == "Flag ▸"));
        assert!(shown.iter().flatten().any(|label| label == "Snooze ▸"));
    }

    #[test]
    fn toggles_read_as_the_action_they_take() {
        let mut unread = facts(None);
        unread.unread = true;
        let mut pinned = facts(None);
        pinned.pinned = true;
        let mut snoozed = facts(None);
        snoozed.snoozed = true;

        let mixed = message_menu_state(&[unread.clone(), pinned.clone()], false, true);
        let third = &labels(&message_menu_description(&mixed, &MenuChoices::default()))[2];
        assert_eq!(third, &vec!["Mark as read", "Flag ▸", "Pin", "Snooze ▸", "Categorize ▸", "Importance ▸"]);

        let all_pinned = message_menu_state(&[pinned.clone(), pinned], false, true);
        assert!(labels(&message_menu_description(&all_pinned, &MenuChoices::default()))[2].contains(&"Unpin".to_string()));

        let waiting = message_menu_state(&[snoozed, facts(None)], false, true);
        let third = &labels(&message_menu_description(&waiting, &MenuChoices::default()))[2];
        assert!(third.contains(&"Unsnooze".to_string()) && !third.contains(&"Snooze ▸".to_string()));
    }

    #[test]
    fn state_follows_folder_role_selection_and_accounts() {
        let inbox = facts(Some(MailboxRole::Inbox));
        let single = message_menu_state(std::slice::from_ref(&inbox), true, true);
        assert!(single.single && single.can_reply && single.can_block && single.can_archive && single.can_move && single.can_snooze && single.can_report_junk);
        assert!(!single.can_not_junk, "Not junk is only offered inside Junk");

        let several = message_menu_state(&[inbox.clone(), inbox.clone()], false, true);
        assert!(!several.single && !several.can_reply && !several.can_block, "single-message verbs need one message");
        assert!(several.can_move);

        let draft = message_menu_state(&[facts(Some(MailboxRole::Drafts))], true, true);
        assert!(!draft.can_reply && !draft.can_snooze && !draft.can_block, "no reply to, snooze of or block by a draft");
        let mut flagged_draft = inbox.clone();
        flagged_draft.draft = true;
        assert!(!message_menu_state(&[flagged_draft], true, true).can_reply, "\\Draft outside Drafts too");

        assert!(!message_menu_state(&[facts(Some(MailboxRole::Junk))], true, true).can_report_junk);
        assert!(message_menu_state(&[facts(Some(MailboxRole::Junk))], true, true).can_not_junk);
        assert!(!message_menu_state(&[facts(Some(MailboxRole::Archive))], true, true).can_archive);
        assert!(!message_menu_state(std::slice::from_ref(&inbox), true, false).can_archive, "no Archive folder");

        let mut elsewhere = inbox.clone();
        elsewhere.account = "other".into();
        let unified = message_menu_state(&[inbox, elsewhere], false, true);
        assert!(!unified.can_move && !unified.can_archive, "folders don't span accounts");

        assert_eq!(message_menu_state(&[], false, true), MessageMenuState::default());
    }

    #[test]
    fn enabled_actions_track_the_state() {
        let state = message_menu_state(&[facts(Some(MailboxRole::Junk)), facts(Some(MailboxRole::Junk))], false, true);
        let enabled: std::collections::HashMap<_, _> = enabled_actions(&state).into_iter().collect();
        assert!(!enabled["reply"] && !enabled["print"] && !enabled["view-source"]);
        assert!(!enabled["report-junk"]);
        assert!(enabled["not-junk"], "Not junk is enabled inside Junk");
        assert!(enabled["move"] && enabled["copy-other"] && enabled["archive"]);
        // Every name it toggles is a declared action.
        let declared: Vec<&str> = action_specs().iter().map(|spec| spec.name).collect();
        for (name, _) in enabled_actions(&state) {
            assert!(declared.contains(&name), "{name} isn't declared");
        }
    }

    #[test]
    fn submenus_carry_their_targets() {
        let projects = MailboxId("acc:Projects".into());
        let due = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        let wake = DateTime::from_timestamp(1_792_000_000, 0).unwrap();
        let choices = MenuChoices {
            recent_folders: vec![(projects.clone(), "Projects".into())],
            flags: vec![
                FlagEntry {
                    name: "Friday".into(),
                    due: Some(due),
                },
                FlagEntry {
                    name: "No date".into(),
                    due: None,
                },
            ],
            snoozes: vec![SnoozeEntry {
                label: "Tomorrow".into(),
                at: wake,
            }],
            report_phishing: false,
            sender_blocked: false,
            focused_inbox: false,
            thread_ignored: false,
        };
        let menu = message_menu_description(&message_menu_state(&[facts(None)], true, true), &choices);
        assert_eq!(labels(submenu(&menu, "Move")), vec![vec!["Projects"], vec!["Other folder…"]]);
        assert_eq!(labels(submenu(&menu, "Copy")), vec![vec!["Projects"], vec!["Other folder…"]]);
        assert_eq!(
            labels(submenu(&menu, "Flag")),
            vec![vec!["Friday", "No date", "Custom date…"], vec!["Mark complete", "Clear flag"]]
        );
        assert_eq!(labels(submenu(&menu, "Snooze")), vec![vec!["Tomorrow"], vec!["Choose a date…"]]);

        // Each target round-trips through `message_action_for`.
        let targets: Vec<(String, Option<glib::Variant>)> = ["Move", "Flag", "Snooze"]
            .iter()
            .flat_map(|name| submenu(&menu, name).sections_for_test().iter().flatten())
            .filter_map(|item| match item {
                MenuItem::Action(action) => Some((action.action.trim_start_matches("msg.").to_string(), action.target.clone())),
                _ => None,
            })
            .collect();
        let actions: Vec<MessageMenuAction> = targets.iter().filter_map(|(name, target)| message_action_for(name, target.as_ref())).collect();
        assert!(actions.contains(&MessageMenuAction::MoveTo(projects)));
        assert!(actions.contains(&MessageMenuAction::Flag(crate::follow_up::flagged(Some(due)))));
        assert!(actions.contains(&MessageMenuAction::Flag(crate::follow_up::flagged(None))));
        assert!(actions.contains(&MessageMenuAction::Snooze(wake)));
        assert!(actions.contains(&MessageMenuAction::Flag(crate::follow_up::completed())));
        assert!(actions.contains(&MessageMenuAction::Flag(crate::follow_up::cleared())));
    }

    #[test]
    fn action_names_map_and_bad_parameters_are_refused() {
        let plain = [
            ("reply", MessageMenuAction::Reply),
            ("reply-all", MessageMenuAction::ReplyAll),
            ("forward", MessageMenuAction::Forward),
            ("reply-with-meeting", MessageMenuAction::ReplyWithMeeting),
            ("delete", MessageMenuAction::Delete),
            ("archive", MessageMenuAction::Archive),
            ("move-other", MessageMenuAction::MoveOther),
            ("copy-other", MessageMenuAction::CopyOther),
            ("mark-read", MessageMenuAction::SetRead(true)),
            ("mark-unread", MessageMenuAction::SetRead(false)),
            ("flag-custom", MessageMenuAction::FlagCustom),
            ("pin", MessageMenuAction::Pin(true)),
            ("unpin", MessageMenuAction::Pin(false)),
            ("snooze-custom", MessageMenuAction::SnoozeCustom),
            ("unsnooze", MessageMenuAction::Unsnooze),
            ("report-junk", MessageMenuAction::ReportJunk),
            ("not-junk", MessageMenuAction::NotJunk),
            ("report-phishing", MessageMenuAction::ReportPhishing),
            ("block-sender", MessageMenuAction::BlockSender),
            ("block-domain", MessageMenuAction::BlockDomain),
            ("unblock-sender", MessageMenuAction::UnblockSender),
            ("ignore", MessageMenuAction::Ignore),
            ("stop-ignoring", MessageMenuAction::StopIgnoring),
            ("clean-up-conversation", MessageMenuAction::CleanUpConversation),
            ("importance-high", MessageMenuAction::SetImportance(Importance::High)),
            ("importance-normal", MessageMenuAction::SetImportance(Importance::Normal)),
            ("importance-low", MessageMenuAction::SetImportance(Importance::Low)),
            ("focus-focused", MessageMenuAction::MoveFocused(true)),
            ("focus-other", MessageMenuAction::MoveFocused(false)),
            ("focus-always-focused", MessageMenuAction::AlwaysMoveFocused(true)),
            ("focus-always-other", MessageMenuAction::AlwaysMoveFocused(false)),
            ("focus-stop", MessageMenuAction::StopMovingFocused),
            ("view-details", MessageMenuAction::ViewDetails),
            ("view-source", MessageMenuAction::ViewSource),
            ("open-window", MessageMenuAction::OpenWindow),
            ("print", MessageMenuAction::Print),
        ];
        for (name, expected) in plain {
            assert_eq!(message_action_for(name, None), Some(expected), "{name}");
        }
        assert_eq!(
            message_action_for("copy", Some(&"acc:Projects".to_variant())),
            Some(MessageMenuAction::CopyTo(MailboxId("acc:Projects".into())))
        );
        assert_eq!(message_action_for("move", Some(&"".to_variant())), None);
        assert_eq!(message_action_for("move", None), None);
        assert_eq!(message_action_for("flag", Some(&"16/10/2026".to_variant())), None);
        assert_eq!(message_action_for("snooze", Some(&"soon".to_variant())), None);
        assert_eq!(message_action_for("ignore", Some(&"x".to_variant())), Some(MessageMenuAction::Ignore));
    }

    /// The description builds into a real popover whose Categorize submenu
    /// has the custom slot the checklist goes into, and every declared
    /// action resolves in its group.
    #[test]
    fn the_menu_builds_with_its_categorize_slot() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let state = message_menu_state(&[facts(Some(MailboxRole::Inbox))], true, true);
        let dispatch: crate::context_menu::Dispatch = std::rc::Rc::new(|_, _| {});
        let menu = crate::context_menu::ContextMenu::new(&message_menu_description(&state, &MenuChoices::default()), &action_specs(), dispatch);
        assert!(menu.popover.add_child(&gtk::Label::new(Some("tags")), CATEGORIZE_SLOT), "the Categorize slot exists");
        for spec in action_specs() {
            assert!(menu.actions.lookup_action(spec.name).is_some(), "{} is in the group", spec.name);
        }
    }

    #[test]
    fn recent_folders_are_most_recent_first_unique_and_capped() {
        let id = |n: usize| MailboxId(format!("acc:F{n}"));
        let mut recent = Vec::new();
        for n in 0..7 {
            push_recent_folder(&mut recent, id(n));
        }
        assert_eq!(recent, vec![id(6), id(5), id(4), id(3), id(2)]);
        push_recent_folder(&mut recent, id(3));
        assert_eq!(recent, vec![id(3), id(6), id(5), id(4), id(2)], "a reused folder moves to the front");
    }
}
