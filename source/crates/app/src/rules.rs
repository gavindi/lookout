//! The client-side rules engine (OUTLOOK_PARITY.md §5.1 M-14, roadmap item
//! 2.5).
//!
//! Rules are evaluated in `lookout-app` on genuinely-new arriving Inbox mail
//! (the session's `AccountEvent::NewMailArrived`) and their actions are
//! dispatched through the existing `AccountCommand`s - the same operations the
//! UI's ribbon and context menus use, so a rule does exactly what a user click
//! does. Matching is the pure `lookout_core::Rule::matches`; this module holds
//! the app-side glue: the master switch, per-account target resolution
//! (a [`RuleTarget`] role/folder -> a `MailboxId`), category-name resolution
//! against the shared master list (item 2.1), and the forward/redirect queue
//! that waits on a message's raw bytes.
//!
//! Scope: like Outlook, client rules run on **arriving Inbox mail**, never on
//! a first sync (the session only emits `NewMailArrived` for genuine
//! arrivals) and never on Sent/Drafts/Junk/Trash/Archive. "Run rules now" over
//! an arbitrary folder is an explicit entry point ([`run_rules_now`]).
//!
//! Body/Header conditions: a message's list-row summary carries neither its
//! body nor arbitrary headers, so when any enabled rule has a Body or Header
//! condition the whole batch is deferred - the bodies are fetched and the
//! rules evaluated on delivery ([`evaluate_deferred_rules`]), preserving rule
//! order. A body that never arrives simply leaves that batch unevaluated.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use lookout_core::{
    AccountId, EmailAddress, EmailBody, EmailSummary, Mailbox, MailboxId, MailboxRole, Rule, RuleAction, RuleBackend, RuleCondition, RuleContext, RuleTarget, SystemFlagBit, Uid,
};
use lookout_mail::session::AccountCommand;
use lookout_mail::{Attachment, ComposedMessage};

use crate::window::{PendingRuleForward, RuleForwardKind, UiState};

/// The most deferred rule evaluations to hold at once; beyond this the oldest
/// are dropped, so a body that never arrives can't grow the queue without
/// bound.
const MAX_PENDING_EVALS: usize = 500;

/// Runs the enabled client-side rules over a batch of genuinely-new arriving
/// messages, dispatching each matching rule's actions as account commands.
pub(crate) fn run_client_rules(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox: &MailboxId, messages: &[EmailSummary]) {
    if messages.is_empty() || !rules_enabled(state) {
        return;
    }
    let Some(run) = RunContext::for_account(state, account_id) else {
        return;
    };
    // Outlook's rule scope is the Inbox; other folders are handled by an
    // explicit "Run rules now".
    let is_inbox = run.folders.iter().any(|folder| &folder.id == mailbox && folder.role == MailboxRole::Inbox);
    if !is_inbox {
        return;
    }
    if run.rules.is_empty() {
        return;
    }
    evaluate_batch(state, &run, messages);
}

/// Applies the account's client rules to every cached message in `mailbox`,
/// the folder menu's "Run rules now". Unlike automatic arrival, this runs on
/// whatever folder the user chose, Inbox or not.
pub(crate) fn run_rules_now(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox: &MailboxId) {
    if !rules_enabled(state) {
        return;
    }
    let Some(run) = RunContext::for_account(state, account_id) else {
        return;
    };
    if run.rules.is_empty() {
        return;
    }
    let messages = {
        let st = state.borrow();
        match st.accounts.get(account_id).and_then(|handle| handle.address_cache.clone()) {
            Some(cache) => cache.load_messages(mailbox).unwrap_or_default(),
            None => Vec::new(),
        }
    };
    if !messages.is_empty() {
        evaluate_batch(state, &run, &messages);
    }
}

/// Whether the rules master switch is on.
fn rules_enabled(state: &Rc<RefCell<UiState>>) -> bool {
    state.borrow().settings.get_bool(crate::settings::MAIL_RULES_ENABLED)
}

/// The per-run snapshot of everything evaluation needs, taken under one
/// borrow so dispatching a command can't deadlock on the state lock.
struct RunContext {
    account_id: AccountId,
    rules: Vec<Rule>,
    folders: Vec<Mailbox>,
    owner: Vec<EmailAddress>,
    account_email: String,
    categories: crate::categories::CategorySet,
    cmd_tx: async_channel::Sender<AccountCommand>,
    interactive_cmd_tx: async_channel::Sender<AccountCommand>,
}

impl RunContext {
    fn for_account(state: &Rc<RefCell<UiState>>, account_id: &AccountId) -> Option<RunContext> {
        let st = state.borrow();
        let handle = st.accounts.get(account_id)?;
        let rules: Vec<Rule> = st
            .app_config
            .borrow()
            .rules
            .iter()
            .filter(|rule| rule.enabled && rule.backend == RuleBackend::Client)
            .cloned()
            .collect();
        let owner = owner_addresses(&st, account_id, &handle.email);
        let folders = handle.folders.clone();
        let account_email = handle.email.clone();
        let categories = st.categories.borrow().clone();
        let cmd_tx = handle.cmd_tx.clone();
        let interactive_cmd_tx = handle.interactive_cmd_tx.clone();
        Some(RunContext {
            account_id: account_id.clone(),
            rules,
            folders,
            owner,
            account_email,
            categories,
            cmd_tx,
            interactive_cmd_tx,
        })
    }
}

/// Evaluates `messages` against `run`. When any rule needs a body or headers,
/// the batch is deferred until the bodies arrive; otherwise it runs at once.
fn evaluate_batch(state: &Rc<RefCell<UiState>>, run: &RunContext, messages: &[EmailSummary]) {
    if run.rules.iter().any(needs_body) {
        let mut st = state.borrow_mut();
        for message in messages {
            st.pending_rule_evals.push(message.clone());
            let _ = run.interactive_cmd_tx.send_blocking(AccountCommand::FetchBody {
                mailbox: message.mailbox.clone(),
                uid: message.uid,
            });
        }
        if st.pending_rule_evals.len() > MAX_PENDING_EVALS {
            let excess = st.pending_rule_evals.len() - MAX_PENDING_EVALS;
            st.pending_rule_evals.drain(0..excess);
        }
        return;
    }
    for message in messages {
        evaluate_message(state, run, message, None);
    }
}

/// Runs the enabled rules over one message (with the fetched body when the
/// deferred path supplies one), executing the matching rules' actions in
/// order and honouring `StopProcessing`.
fn evaluate_message(state: &Rc<RefCell<UiState>>, run: &RunContext, message: &EmailSummary, body: Option<&EmailBody>) {
    let fallback_headers: &[(String, String)] = &[];
    let context = RuleContext {
        body: body.and_then(|body| body.text_body.as_deref()),
        headers: body.map(|body| body.headers.as_slice()).unwrap_or(fallback_headers),
        owner: &run.owner,
        date: None,
    };
    let mut stop = false;
    for rule in &run.rules {
        if !rule.matches_for_account(&run.account_id, message, &context) {
            continue;
        }
        for action in &rule.actions {
            match action {
                RuleAction::Move(target) => {
                    if let Some(target) = resolve_target(&run.folders, target) {
                        if target != message.mailbox {
                            let _ = run.cmd_tx.send_blocking(AccountCommand::MoveMessagesTo {
                                mailbox: message.mailbox.clone(),
                                uids: vec![message.uid],
                                target,
                            });
                        }
                    }
                }
                RuleAction::Copy(target) => {
                    if let Some(target) = resolve_target(&run.folders, target) {
                        let _ = run.cmd_tx.send_blocking(AccountCommand::CopyMessagesTo {
                            mailbox: message.mailbox.clone(),
                            uids: vec![message.uid],
                            target,
                        });
                    }
                }
                RuleAction::Delete => {
                    let _ = run.cmd_tx.send_blocking(AccountCommand::MoveMessage {
                        mailbox: message.mailbox.clone(),
                        uid: message.uid,
                        role: MailboxRole::Trash,
                    });
                }
                RuleAction::Flag(flag) => {
                    let _ = run.cmd_tx.send_blocking(AccountCommand::SetFollowUp {
                        mailbox: message.mailbox.clone(),
                        uids: vec![message.uid],
                        flag: flag.clone(),
                    });
                }
                RuleAction::Categorize(names) => {
                    let keywords: Vec<String> = names
                        .iter()
                        .filter_map(|name| run.categories.find_by_name(name).map(|category| category.keyword()))
                        .collect();
                    if !keywords.is_empty() {
                        let _ = run.cmd_tx.send_blocking(AccountCommand::StoreKeywordsMany {
                            mailbox: message.mailbox.clone(),
                            uids: vec![message.uid],
                            add: keywords,
                            remove: Vec::new(),
                        });
                    }
                }
                RuleAction::MarkRead(read) => {
                    let (add, remove) = if *read {
                        (vec![SystemFlagBit::Seen], Vec::new())
                    } else {
                        (Vec::new(), vec![SystemFlagBit::Seen])
                    };
                    let _ = run.cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
                        mailbox: message.mailbox.clone(),
                        uids: vec![message.uid],
                        add,
                        remove,
                    });
                }
                RuleAction::Forward(to) => queue_forward(state, run, message, to, RuleForwardKind::Forward),
                RuleAction::Redirect(to) => queue_forward(state, run, message, to, RuleForwardKind::Redirect),
                RuleAction::StopProcessing => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            break;
        }
    }
}

/// Evaluates the deferred rule batch for a message whose body just arrived,
/// called from the `BodyFetched` handler.
pub(crate) fn evaluate_deferred_rules(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox: &MailboxId, uid: Uid, body: &EmailBody) {
    let pending: Vec<EmailSummary> = {
        let mut st = state.borrow_mut();
        let mut taken = Vec::new();
        let mut kept = Vec::new();
        for summary in st.pending_rule_evals.drain(..) {
            if &summary.mailbox == mailbox && summary.uid == uid {
                taken.push(summary);
            } else {
                kept.push(summary);
            }
        }
        st.pending_rule_evals = kept;
        taken
    };
    if pending.is_empty() {
        return;
    }
    let Some(run) = RunContext::for_account(state, account_id) else {
        return;
    };
    for message in &pending {
        evaluate_message(state, &run, message, Some(body));
    }
}

/// Whether a rule has a condition the list-row summary can't evaluate.
fn needs_body(rule: &Rule) -> bool {
    rule.conditions
        .iter()
        .chain(rule.exceptions.iter())
        .any(|condition| matches!(condition, RuleCondition::Body(_) | RuleCondition::Header { .. }))
}

/// The owner's own addresses for the "sent only to me" condition: the
/// account's primary address plus any configured sending identities.
fn owner_addresses(state: &crate::window::UiState, account_id: &AccountId, account_email: &str) -> Vec<EmailAddress> {
    let mut addresses = vec![EmailAddress::new(account_email)];
    for identity in state.app_config.borrow().identities_for(account_id) {
        addresses.push(EmailAddress::new(identity.email));
    }
    addresses
}

/// Resolves a rule action's target against one account's folder list. A role
/// resolves to the folder carrying that special-use role; a folder name
/// matches case-insensitively against the display name. `None` when the
/// account has no matching folder (the action is skipped, not an error).
pub(crate) fn resolve_target(folders: &[Mailbox], target: &RuleTarget) -> Option<MailboxId> {
    match target {
        RuleTarget::Role(role) => folders.iter().find(|folder| folder.role == *role).map(|folder| folder.id.clone()),
        RuleTarget::Folder(name) => folders.iter().find(|folder| folder.name.eq_ignore_ascii_case(name)).map(|folder| folder.id.clone()),
    }
}

/// Queues a rule forward/redirect and asks the session for the message's raw
/// bytes; `deliver_rule_forwards` sends it once they arrive. Messages the
/// account itself sent are skipped, so a forward-to-self rule can't loop.
fn queue_forward(state: &Rc<RefCell<UiState>>, run: &RunContext, message: &EmailSummary, to: &[EmailAddress], kind: RuleForwardKind) {
    if to.is_empty() {
        return;
    }
    if message.from.iter().any(|address| address.address.eq_ignore_ascii_case(&run.account_email)) {
        return;
    }
    let to: Vec<String> = to.iter().map(|address| address.address.clone()).collect();
    let mut st = state.borrow_mut();
    st.pending_rule_forwards.push(PendingRuleForward {
        mailbox: message.mailbox.clone(),
        uid: message.uid,
        to,
        subject: message.subject.clone(),
        kind,
    });
    let _ = run.interactive_cmd_tx.send_blocking(AccountCommand::FetchRawMessage {
        mailbox: message.mailbox.clone(),
        uid: message.uid,
    });
}

/// Sends every rule forward/redirect waiting on `(mailbox, uid)`'s raw bytes,
/// called from the `RawMessageFetched` handler.
pub(crate) fn deliver_rule_forwards(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, raw: &[u8]) {
    let pending = take_pending(state, mailbox, uid);
    for forward in pending {
        let Some(account_id) = crate::window::mailbox_account_id(mailbox) else {
            continue;
        };
        let Some((from, display_name, cmd_tx)) = ({
            let st = state.borrow();
            st.accounts
                .get(&account_id)
                .map(|handle| (handle.email.clone(), handle.display_name.clone(), handle.cmd_tx.clone()))
        }) else {
            continue;
        };
        let message = build_rule_forward(&from, &display_name, &forward, raw);
        let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
            message: Box::new(message),
            not_before: None,
            client_token: None,
        });
    }
}

/// Drops every rule forward/redirect waiting on `(mailbox, uid)` when their
/// raw fetch fails.
pub(crate) fn fail_rule_forwards(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, _message: &str) {
    let _ = take_pending(state, mailbox, uid);
}

/// Removes and returns the pending forwards for one message, leaving the rest
/// queued.
fn take_pending(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid) -> Vec<PendingRuleForward> {
    let mut st = state.borrow_mut();
    let mut taken = Vec::new();
    let mut kept = Vec::new();
    for forward in st.pending_rule_forwards.drain(..) {
        if &forward.mailbox == mailbox && forward.uid == uid {
            taken.push(forward);
        } else {
            kept.push(forward);
        }
    }
    st.pending_rule_forwards = kept;
    taken
}

/// Builds the message a rule forward/redirect sends. Both carry the original
/// as a `message/rfc822` attachment so nothing is lost; a forward gets a
/// `Fwd:` subject prefix, a redirect keeps the subject unchanged.
pub(crate) fn build_rule_forward(from: &str, display_name: &str, pending: &PendingRuleForward, raw: &[u8]) -> ComposedMessage {
    let subject = pending.subject.clone().unwrap_or_default();
    let (subject, text_body) = match pending.kind {
        RuleForwardKind::Forward => {
            let subject = if subject.trim().is_empty() {
                "Fwd:".to_string()
            } else if subject.to_ascii_lowercase().starts_with("fwd:") {
                subject
            } else {
                format!("Fwd: {subject}")
            };
            (subject, "Forwarded automatically by a rule.".to_string())
        }
        RuleForwardKind::Redirect => (subject, String::new()),
    };
    ComposedMessage {
        from: from.to_string(),
        display_name: (!display_name.trim().is_empty()).then(|| display_name.to_string()),
        to: pending.to.clone(),
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject,
        importance: lookout_core::Importance::Normal,
        sensitivity: lookout_core::MessageSensitivity::Normal,
        text_body,
        html_body: None,
        attachments: vec![Attachment {
            filename: "forwarded-message.eml".to_string(),
            content_type: "message/rfc822".to_string(),
            bytes: raw.to_vec(),
        }],
        inline_images: Vec::new(),
        calendar_part: None,
        read_receipt: None,
        request_read_receipt: false,
        in_reply_to: None,
        references: Vec::new(),
        message_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::UidValidity;

    fn folder(id: &str, name: &str, role: MailboxRole) -> Mailbox {
        Mailbox {
            id: MailboxId(format!("acct:{id}")),
            account_id: AccountId("acct".into()),
            name: name.to_string(),
            parent: None,
            delimiter: '/',
            role,
            uidvalidity: UidValidity(1),
            uidnext: 1,
            highest_modseq: None,
            total: 0,
            unread: 0,
            flags: Vec::new(),
            subscribed: true,
        }
    }

    #[test]
    fn resolves_role_and_folder_targets() {
        let folders = vec![folder("INBOX", "Inbox", MailboxRole::Inbox), folder("Archive", "Archive 2024", MailboxRole::Archive)];
        assert_eq!(resolve_target(&folders, &RuleTarget::Role(MailboxRole::Archive)).unwrap().0, "acct:Archive");
        assert_eq!(resolve_target(&folders, &RuleTarget::Folder("archive 2024".into())).unwrap().0, "acct:Archive");
        assert!(resolve_target(&folders, &RuleTarget::Folder("Missing".into())).is_none());
        // A role the account doesn't have resolves to nothing.
        assert!(resolve_target(&folders, &RuleTarget::Role(MailboxRole::Junk)).is_none());
    }

    #[test]
    fn needs_body_recognises_body_and_header_conditions() {
        let mut rule = Rule::new("r", "Newsletters");
        rule.conditions = vec![RuleCondition::From("x".into())];
        assert!(!needs_body(&rule));
        rule.conditions.push(RuleCondition::Body("unsubscribe".into()));
        assert!(needs_body(&rule));
        let mut rule = Rule::new("r2", "Spam");
        rule.exceptions = vec![RuleCondition::Header {
            name: "X-Spam".into(),
            value: "no".into(),
        }];
        assert!(needs_body(&rule));
    }

    #[test]
    fn forward_subject_is_prefixed_once() {
        let pending = PendingRuleForward {
            mailbox: MailboxId("acct:INBOX".into()),
            uid: Uid(1),
            to: vec!["a@example.com".into()],
            subject: Some("Quarterly report".into()),
            kind: RuleForwardKind::Forward,
        };
        let message = build_rule_forward("me@example.com", "Me", &pending, b"raw");
        assert_eq!(message.subject, "Fwd: Quarterly report");
        assert_eq!(message.to, vec!["a@example.com".to_string()]);
        assert_eq!(message.attachments[0].content_type, "message/rfc822");

        let pending = PendingRuleForward {
            subject: Some("Fwd: Already".into()),
            ..pending
        };
        assert_eq!(build_rule_forward("me@example.com", "Me", &pending, b"raw").subject, "Fwd: Already");
    }

    #[test]
    fn redirect_keeps_the_subject_and_body_empty() {
        let pending = PendingRuleForward {
            mailbox: MailboxId("acct:INBOX".into()),
            uid: Uid(1),
            to: vec!["a@example.com".into()],
            subject: Some("Budget".into()),
            kind: RuleForwardKind::Redirect,
        };
        let message = build_rule_forward("me@example.com", "", &pending, b"raw");
        assert_eq!(message.subject, "Budget");
        assert!(message.text_body.is_empty());
        assert_eq!(message.display_name, None);
    }
}
