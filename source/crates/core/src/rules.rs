//! The rules engine's pure domain model (OUTLOOK_PARITY.md §5.1 M-14, roadmap
//! item 2.5).
//!
//! A [`Rule`] is a name plus an ordered list of conditions, exceptions and
//! actions, evaluated in list order. The same model backs all three of item
//! 2.5's backends - the client engine (in `lookout-app`), a server Sieve script
//! and Microsoft Graph `messageRules` - so this module holds only the shared
//! shape and the pure in-memory matcher. It has no I/O, matching
//! `lookout-core`'s charter.
//!
//! [`Rule::matches`] is the client-side evaluation: every condition must hold
//! ([`RuleCondition`]) and no exception may
//! ([`Rule::exceptions`]). Conditions that need data beyond the list-row
//! [`EmailSummary`] - a message body or an arbitrary header - are supplied
//! through a [`RuleContext`]; when that data is absent the condition simply
//! doesn't match, so a rule never fires on information it doesn't have.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Utc};

use crate::email::{EmailAddress, EmailSummary};
use crate::interop::{FollowUpFlag, Importance};
use crate::mailbox::MailboxRole;

/// Where a rule is executed (item 2.5). `Client` runs in `lookout-app` on new
/// mail; `Sieve` is uploaded to a ManageSieve server (RFC 5804); `Graph` is
/// Microsoft 365's `messageRules`. The UI shows this per rule; the engine
/// degrades to `Client` when a backend is unavailable (§10, "degrade softly").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RuleBackend {
    #[default]
    Client,
    Sieve,
    Graph,
}

impl RuleBackend {
    /// The user-facing label ("where it runs").
    pub fn label(self) -> &'static str {
        match self {
            RuleBackend::Client => "This device",
            RuleBackend::Sieve => "Server (Sieve)",
            RuleBackend::Graph => "Microsoft 365",
        }
    }
}

/// A comparison used by the size condition, mirroring the mail search
/// language's `size:` operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SizeComparison {
    LessThan,
    LessOrEqual,
    GreaterThan,
    GreaterOrEqual,
    Equal,
}

impl SizeComparison {
    /// The operator's user-facing symbol.
    pub fn symbol(self) -> &'static str {
        match self {
            SizeComparison::LessThan => "<",
            SizeComparison::LessOrEqual => "≤",
            SizeComparison::GreaterThan => ">",
            SizeComparison::GreaterOrEqual => "≥",
            SizeComparison::Equal => "=",
        }
    }

    fn holds(self, size: u32, bound: u64) -> bool {
        let size = size as u64;
        match self {
            SizeComparison::LessThan => size < bound,
            SizeComparison::LessOrEqual => size <= bound,
            SizeComparison::GreaterThan => size > bound,
            SizeComparison::GreaterOrEqual => size >= bound,
            SizeComparison::Equal => size == bound,
        }
    }
}

/// One condition a message must satisfy for a rule to apply. String operators
/// are case-insensitive substring matches, consistent with the search query
/// language (`mail/src/search_query.rs`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuleCondition {
    /// The sender address or display name contains this text.
    From(String),
    /// A `To` recipient contains this text.
    To(String),
    /// A `Cc` recipient contains this text.
    Cc(String),
    /// The subject contains this text.
    Subject(String),
    /// The message body contains this text. Needs [`RuleContext::body`]; else
    /// never matches.
    Body(String),
    /// A header with this name contains this text. Needs
    /// [`RuleContext::headers`]; else never matches.
    Header { name: String, value: String },
    /// The message has (or lacks) an attachment.
    HasAttachment(bool),
    /// The message's importance equals this value.
    Importance(Importance),
    /// The message's size compares to this byte count.
    Size { comparison: SizeComparison, bytes: u64 },
    /// The message is addressed only to the account owner (Outlook's "sent only
    /// to me"): the `To` box is non-empty, every address in it is the owner's,
    /// and `Cc` is empty. Needs [`RuleContext::owner`]; else never matches.
    SentOnlyToMe,
    /// The message's RFC 2919 `List-Id` contains this text.
    ListId(String),
}

impl RuleCondition {
    /// A short, user-facing description (the rule editor and the rule list).
    pub fn label(&self) -> String {
        match self {
            RuleCondition::From(v) => format!("From contains \"{v}\""),
            RuleCondition::To(v) => format!("To contains \"{v}\""),
            RuleCondition::Cc(v) => format!("Cc contains \"{v}\""),
            RuleCondition::Subject(v) => format!("Subject contains \"{v}\""),
            RuleCondition::Body(v) => format!("Body contains \"{v}\""),
            RuleCondition::Header { name, value } => format!("Header \"{name}\" contains \"{value}\""),
            RuleCondition::HasAttachment(true) => "Has an attachment".to_string(),
            RuleCondition::HasAttachment(false) => "Has no attachment".to_string(),
            RuleCondition::Importance(i) => format!("Importance is {}", importance_label(*i)),
            RuleCondition::Size { comparison, bytes } => format!("Size is {} {}", comparison.symbol(), human_size(*bytes)),
            RuleCondition::SentOnlyToMe => "Sent only to me".to_string(),
            RuleCondition::ListId(v) => format!("Mailing list contains \"{v}\""),
        }
    }

    /// Whether this condition is satisfied by `message` given `context`.
    pub fn matches(&self, message: &EmailSummary, context: &RuleContext<'_>) -> bool {
        match self {
            RuleCondition::From(text) => addresses_contain_text(&message.from, text),
            RuleCondition::To(text) => addresses_contain_text(&message.to, text),
            RuleCondition::Cc(text) => addresses_contain_text(&message.cc, text),
            RuleCondition::Subject(text) => contains_ci(message.subject.as_deref().unwrap_or_default(), text),
            RuleCondition::Body(text) => context.body.is_some_and(|body| contains_ci(body, text)),
            // An empty `name` matches any header (Graph's `headerContains` has
            // no header name of its own); otherwise the name must match too.
            RuleCondition::Header { name, value } => context
                .headers
                .iter()
                .any(|(h, v)| (name.trim().is_empty() || h.eq_ignore_ascii_case(name)) && contains_ci(v, value)),
            RuleCondition::HasAttachment(want) => message.has_attachment == *want,
            RuleCondition::Importance(want) => message.importance == *want,
            RuleCondition::Size { comparison, bytes } => comparison.holds(message.size, *bytes),
            RuleCondition::SentOnlyToMe => sent_only_to(message, context.owner),
            RuleCondition::ListId(text) => message.list_id.as_deref().is_some_and(|list| contains_ci(list, text)),
        }
    }
}

/// The target of a move or copy action. A [`MailboxRole`] resolves against
/// whichever account the message belongs to (portable across accounts); a
/// folder path names a specific mailbox and is skipped when absent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuleTarget {
    Role(MailboxRole),
    Folder(String),
}

impl RuleTarget {
    /// A short, user-facing description.
    pub fn label(&self) -> String {
        match self {
            RuleTarget::Role(role) => format!("{} folder", role_label(*role)),
            RuleTarget::Folder(folder) => folder.clone(),
        }
    }
}

/// One thing a rule does to a matching message. Actions run in list order;
/// [`RuleAction::StopProcessing`] ends the whole evaluation (no later rule
/// runs) as Outlook's "stop processing more rules" does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuleAction {
    Move(RuleTarget),
    Copy(RuleTarget),
    Delete,
    /// Apply a follow-up flag (M-10).
    Flag(FollowUpFlag),
    /// Apply categories by name (the shared master list, item 2.1).
    Categorize(Vec<String>),
    MarkRead(bool),
    /// Forward the message, quoting/attaching as the composer does.
    Forward(Vec<EmailAddress>),
    /// Redirect the message (RFC 5322 resent form) without changing the sender.
    Redirect(Vec<EmailAddress>),
    /// Stop evaluating further rules for this message.
    StopProcessing,
}

impl RuleAction {
    /// A short, user-facing description.
    pub fn label(&self) -> String {
        match self {
            RuleAction::Move(target) => format!("Move to {}", target.label()),
            RuleAction::Copy(target) => format!("Copy to {}", target.label()),
            RuleAction::Delete => "Delete".to_string(),
            RuleAction::Flag(flag) => match flag.status {
                crate::interop::FlagStatus::Complete => "Mark flag complete".to_string(),
                crate::interop::FlagStatus::Flagged => match flag.due {
                    Some(due) => format!("Flag for follow-up, due {}", due.format("%Y-%m-%d")),
                    None => "Flag for follow-up".to_string(),
                },
                crate::interop::FlagStatus::NotFlagged => "Clear flag".to_string(),
            },
            RuleAction::Categorize(names) => format!("Categorize as {}", names.join(", ")),
            RuleAction::MarkRead(true) => "Mark as read".to_string(),
            RuleAction::MarkRead(false) => "Mark as unread".to_string(),
            RuleAction::Forward(to) => format!("Forward to {}", join_addresses(to)),
            RuleAction::Redirect(to) => format!("Redirect to {}", join_addresses(to)),
            RuleAction::StopProcessing => "Stop processing more rules".to_string(),
        }
    }

    /// Whether the action removes the message from its folder (so subsequent
    /// actions should be expressed against the moved message).
    pub fn is_move(&self) -> bool {
        matches!(self, RuleAction::Move(_) | RuleAction::Delete)
    }
}

/// A single rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rule {
    /// Stable identity, used by the editor and by server-side sync. For a
    /// Graph-backed rule this is the Graph `messageRule` id.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub backend: RuleBackend,
    /// The account this rule belongs to, or `None` to apply on every account.
    /// A `Graph` rule always names its Microsoft 365 account (its rules live in
    /// that mailbox); a client rule with an account only runs for that account.
    #[serde(default)]
    pub account: Option<crate::ids::AccountId>,
    /// Every condition must hold (an empty list matches every message).
    pub conditions: Vec<RuleCondition>,
    /// If any exception holds, the rule does not apply (Outlook's "except …").
    #[serde(default)]
    pub exceptions: Vec<RuleCondition>,
    /// Executed in order.
    pub actions: Vec<RuleAction>,
}

impl Rule {
    /// A new, enabled client rule with no conditions or actions.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Rule {
            id: id.into(),
            name: name.into(),
            enabled: true,
            backend: RuleBackend::Client,
            account: None,
            conditions: Vec::new(),
            exceptions: Vec::new(),
            actions: Vec::new(),
        }
    }

    /// Whether the rule applies to `message` from `account`: enabled, scoped
    /// to this account (or to all), every condition holds, and no exception
    /// holds.
    pub fn matches_for_account(&self, account: &crate::ids::AccountId, message: &EmailSummary, context: &RuleContext<'_>) -> bool {
        if self.account.as_ref().is_some_and(|rule_account| rule_account != account) {
            return false;
        }
        self.matches(message, context)
    }

    /// Whether the rule applies to `message`: enabled, every condition holds,
    /// and no exception holds.
    pub fn matches(&self, message: &EmailSummary, context: &RuleContext<'_>) -> bool {
        if !self.enabled {
            return false;
        }
        if !self.conditions.iter().all(|condition| condition.matches(message, context)) {
            return false;
        }
        !self.exceptions.iter().any(|condition| condition.matches(message, context))
    }

    /// Whether the rule ends evaluation for a matching message.
    pub fn stops_processing(&self) -> bool {
        self.actions.iter().any(|action| matches!(action, RuleAction::StopProcessing))
    }
}

/// The extra message data the matcher needs that the list-row [`EmailSummary`]
/// doesn't carry. Everything is optional: a condition on absent data never
/// matches, so the client engine can evaluate the cheap conditions immediately
/// and the rest once a body or header block is available.
#[derive(Debug, Clone, Default)]
pub struct RuleContext<'a> {
    /// The decoded message body (plain text), when fetched.
    pub body: Option<&'a str>,
    /// The message's headers as `(name, value)`, when fetched.
    pub headers: &'a [(String, String)],
    /// The owner's own addresses, for [`RuleCondition::SentOnlyToMe`].
    pub owner: &'a [EmailAddress],
    /// The message's date (unused by the current conditions; reserved for
    /// time-based conditions and kept so the signature stays stable).
    pub date: Option<DateTime<Utc>>,
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn addresses_contain_text(addresses: &[EmailAddress], text: &str) -> bool {
    addresses
        .iter()
        .any(|address| contains_ci(&address.address, text) || address.name.as_deref().is_some_and(|name| contains_ci(name, text)))
}

fn is_owner(address: &EmailAddress, owner: &[EmailAddress]) -> bool {
    owner.iter().any(|o| o.address.eq_ignore_ascii_case(&address.address))
}

/// Outlook's "sent only to me": the `To` box is non-empty, every address in it
/// is the owner's, and `Cc` is empty. With no known owner addresses this is
/// never satisfied.
fn sent_only_to(message: &EmailSummary, owner: &[EmailAddress]) -> bool {
    !owner.is_empty() && !message.to.is_empty() && message.cc.is_empty() && message.to.iter().all(|address| is_owner(address, owner))
}

fn importance_label(importance: Importance) -> &'static str {
    match importance {
        Importance::Low => "Low",
        Importance::Normal => "Normal",
        Importance::High => "High",
    }
}

fn role_label(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "Inbox",
        MailboxRole::Sent => "Sent Items",
        MailboxRole::Drafts => "Drafts",
        MailboxRole::Trash => "Deleted Items",
        MailboxRole::Junk => "Junk Email",
        MailboxRole::Archive => "Archive",
        MailboxRole::Custom => "folder",
    }
}

fn join_addresses(addresses: &[EmailAddress]) -> String {
    addresses.iter().map(|a| a.address.as_str()).collect::<Vec<_>>().join(", ")
}

/// A human-readable byte size ("2.0 MB") for the size condition's label.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AccountId, MailboxId, Uid};
    use crate::thread::ThreadKey;

    fn summary() -> EmailSummary {
        EmailSummary {
            uid: Uid(1),
            mailbox: MailboxId::new(&AccountId("acct".into()), "INBOX"),
            message_id: Some("<m1@example.com>".into()),
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey("m1@example.com".into()),
            subject: Some("Quarterly report".into()),
            from: vec![EmailAddress {
                name: Some("Alice Example".into()),
                address: "alice@example.com".into(),
            }],
            to: vec![EmailAddress::new("me@example.com")],
            cc: Vec::new(),
            date: "2026-10-14T09:00:00Z".parse().unwrap(),
            flags: std::collections::BTreeSet::new(),
            keywords: std::collections::BTreeSet::new(),
            size: 2 * 1024 * 1024,
            has_attachment: true,
            has_calendar: false,
            preview: None,
            importance: Importance::High,
            sensitivity: crate::interop::MessageSensitivity::Normal,
            list_id: Some("team.example.com".into()),
            structure: None,
        }
    }

    fn owner() -> Vec<EmailAddress> {
        vec![EmailAddress::new("me@example.com")]
    }

    fn context<'a>(owner: &'a [EmailAddress]) -> RuleContext<'a> {
        RuleContext { owner, ..RuleContext::default() }
    }

    #[test]
    fn empty_conditions_match_and_exceptions_exclude() {
        let mut rule = Rule::new("r1", "All mail");
        assert!(rule.matches(&summary(), &context(&owner())));
        rule.enabled = false;
        assert!(!rule.matches(&summary(), &context(&owner())), "a disabled rule never matches");
        rule.enabled = true;
        rule.exceptions.push(RuleCondition::Subject("report".into()));
        assert!(!rule.matches(&summary(), &context(&owner())), "an exception excludes the message");
    }

    #[test]
    fn address_conditions_match_name_or_address_case_insensitively() {
        let message = summary();
        assert!(RuleCondition::From("ALICE@example.com".into()).matches(&message, &context(&owner())));
        assert!(RuleCondition::From("alice example".into()).matches(&message, &context(&owner())));
        assert!(RuleCondition::To("me@example.com".into()).matches(&message, &context(&owner())));
        assert!(!RuleCondition::Cc("anyone".into()).matches(&message, &context(&owner())));
        assert!(RuleCondition::Subject("quarterly".into()).matches(&message, &context(&owner())));
        assert!(RuleCondition::ListId("team.example.com".into()).matches(&message, &context(&owner())));
    }

    #[test]
    fn body_and_header_conditions_need_context() {
        let message = summary();
        let cond = RuleCondition::Body("please review".into());
        assert!(!cond.matches(&message, &context(&owner())), "no body -> no match");
        let headers = vec![("X-Spam-Flag".to_string(), "YES".to_string())];
        let ctx = RuleContext {
            body: Some("Please review the draft"),
            headers: &headers,
            owner: &owner(),
            date: None,
        };
        assert!(cond.matches(&message, &ctx));
        assert!(RuleCondition::Header {
            name: "x-spam-flag".into(),
            value: "yes".into()
        }
        .matches(&message, &ctx));
        assert!(!RuleCondition::Header {
            name: "X-Missing".into(),
            value: "yes".into()
        }
        .matches(&message, &ctx));
    }

    #[test]
    fn attachment_importance_and_size_conditions() {
        let message = summary();
        assert!(RuleCondition::HasAttachment(true).matches(&message, &context(&owner())));
        assert!(!RuleCondition::HasAttachment(false).matches(&message, &context(&owner())));
        assert!(RuleCondition::Importance(Importance::High).matches(&message, &context(&owner())));
        assert!(!RuleCondition::Importance(Importance::Low).matches(&message, &context(&owner())));
        assert!(RuleCondition::Size {
            comparison: SizeComparison::GreaterThan,
            bytes: 1024
        }
        .matches(&message, &context(&owner())));
        assert!(RuleCondition::Size {
            comparison: SizeComparison::GreaterOrEqual,
            bytes: 2 * 1024 * 1024
        }
        .matches(&message, &context(&owner())));
        assert!(!RuleCondition::Size {
            comparison: SizeComparison::LessThan,
            bytes: 1024
        }
        .matches(&message, &context(&owner())));
    }

    #[test]
    fn sent_only_to_me() {
        let mut message = summary();
        assert!(RuleCondition::SentOnlyToMe.matches(&message, &context(&owner())));
        // Adding a Cc (or another recipient) breaks the condition.
        message.cc.push(EmailAddress::new("team@example.com"));
        assert!(!RuleCondition::SentOnlyToMe.matches(&message, &context(&owner())));
        let mut message = summary();
        message.to.push(EmailAddress::new("other@example.com"));
        assert!(!RuleCondition::SentOnlyToMe.matches(&message, &context(&owner())));
        // No owner configured -> never matches.
        assert!(!RuleCondition::SentOnlyToMe.matches(&summary(), &context(&[])));
    }

    #[test]
    fn rule_executes_all_conditions_in_order() {
        let rule = Rule {
            id: "r".into(),
            name: "Big high-importance mail".into(),
            enabled: true,
            backend: RuleBackend::Client,
            account: None,
            conditions: vec![
                RuleCondition::Importance(Importance::High),
                RuleCondition::Size {
                    comparison: SizeComparison::GreaterThan,
                    bytes: 1024 * 1024,
                },
            ],
            exceptions: Vec::new(),
            actions: vec![RuleAction::StopProcessing],
        };
        assert!(rule.matches(&summary(), &context(&owner())));
        assert!(rule.stops_processing());
    }

    #[test]
    fn stops_processing_and_move_helpers() {
        assert!(RuleAction::Delete.is_move());
        assert!(RuleAction::Move(RuleTarget::Role(MailboxRole::Archive)).is_move());
        assert!(!RuleAction::Copy(RuleTarget::Folder("Projects".into())).is_move());
        let mut rule = Rule::new("r", "Move then stop");
        rule.actions = vec![RuleAction::Move(RuleTarget::Role(MailboxRole::Archive))];
        assert!(!rule.stops_processing());
        rule.actions.push(RuleAction::StopProcessing);
        assert!(rule.stops_processing());
    }

    #[test]
    fn serializes_round_trip() {
        let rule = Rule {
            id: "r1".into(),
            name: "Archive newsletters".into(),
            enabled: true,
            backend: RuleBackend::Client,
            account: None,
            conditions: vec![
                RuleCondition::ListId("news.example.com".into()),
                RuleCondition::Header {
                    name: "List-Unsubscribe".into(),
                    value: "<".into(),
                },
            ],
            exceptions: vec![RuleCondition::From("vip@example.com".into())],
            actions: vec![
                RuleAction::Categorize(vec!["Newsletters".into()]),
                RuleAction::MarkRead(true),
                RuleAction::Move(RuleTarget::Role(MailboxRole::Archive)),
            ],
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: Rule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);
    }
}
