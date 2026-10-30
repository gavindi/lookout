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

/// Moves genuinely-new arriving mail whose sender is on the account's blocked
/// list (item 2.9 / M-17) to Junk, setting the RFC 5788 `$Junk` keyword so the
/// server learns. Unlike [`run_client_rules`] this is independent of the rules
/// master switch (Outlook's blocked senders always apply) and of any
/// user-defined rules, so it is a separate entry point called alongside it on
/// `NewMailArrived`. Silent: like Outlook's automatic filtering, arrival-time
/// blocking shows no toast and records no undo entry.
pub(crate) fn enforce_blocked_senders(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox: &MailboxId, messages: &[EmailSummary]) {
    if messages.is_empty() {
        return;
    }
    let (blocked, cmd_tx) = {
        let st = state.borrow();
        let Some(handle) = st.accounts.get(account_id) else {
            return;
        };
        let blocked: Vec<String> = st
            .blocked_senders
            .iter()
            .filter(|(account, _)| account == account_id)
            .map(|(_, entry)| entry.clone())
            .collect();
        if blocked.is_empty() {
            return;
        }
        (blocked, handle.cmd_tx.clone())
    };
    let uids: Vec<Uid> = messages
        .iter()
        .filter(|message| {
            message
                .from
                .first()
                .is_some_and(|sender| blocked.iter().any(|entry| lookout_core::sender_matches_trust_entry(&sender.address, entry)))
        })
        .map(|message| message.uid)
        .collect();
    if uids.is_empty() {
        return;
    }
    let _ = cmd_tx.send_blocking(AccountCommand::SetJunk {
        mailbox: mailbox.clone(),
        uids: uids.clone(),
        junk: true,
    });
    let _ = cmd_tx.send_blocking(AccountCommand::MoveMessages {
        mailbox: mailbox.clone(),
        uids,
        role: MailboxRole::Junk,
    });
}

/// Moves genuinely-new arriving mail in an ignored conversation (item 2.8 /
/// M-20) to Deleted Items. Like [`enforce_blocked_senders`] this is
/// independent of the rules master switch (Outlook's Ignore always applies)
/// and silent - arrival-time filtering shows no toast and records no undo.
pub(crate) fn enforce_ignored_threads(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox: &MailboxId, messages: &[EmailSummary]) {
    if messages.is_empty() {
        return;
    }
    let (ignored, cmd_tx) = {
        let st = state.borrow();
        if st.ignored_threads.is_empty() {
            return;
        }
        let Some(handle) = st.accounts.get(account_id) else {
            return;
        };
        let ignored: std::collections::HashSet<String> = st
            .ignored_threads
            .iter()
            .filter(|(account, _)| account == account_id)
            .map(|(_, thread)| thread.clone())
            .collect();
        (ignored, handle.cmd_tx.clone())
    };
    let uids: Vec<Uid> = messages
        .iter()
        .filter(|message| ignored.contains(&message.thread_key.0))
        .map(|message| message.uid)
        .collect();
    if uids.is_empty() {
        return;
    }
    let _ = cmd_tx.send_blocking(AccountCommand::MoveMessages {
        mailbox: mailbox.clone(),
        uids,
        role: MailboxRole::Trash,
    });
}

/// Remembers `thread` as ignored for `account` (item 2.8 / M-20), persisting it
/// so arrival enforcement and the management dialog both see it. `label` is the
/// conversation's subject, shown in the dialog.
pub(crate) fn ignore_thread(state: &Rc<RefCell<UiState>>, account: &AccountId, thread: &str, label: &str) {
    let mut st = state.borrow_mut();
    st.ignored_threads.insert((account.clone(), thread.to_string()));
    if let Some(db) = &st.ui_db {
        let _ = db.set_ignored_thread(account, thread, label);
    }
}

/// Stops ignoring `thread` for `account` (item 2.8 / M-20).
pub(crate) fn unignore_thread(state: &Rc<RefCell<UiState>>, account: &AccountId, thread: &str) {
    let mut st = state.borrow_mut();
    st.ignored_threads.remove(&(account.clone(), thread.to_string()));
    if let Some(db) = &st.ui_db {
        let _ = db.remove_ignored_thread(account, thread);
    }
}

/// Applies the account's client rules to every cached message in `mailbox`,
/// the folder menu's "Run rules now". Unlike automatic arrival, this runs on
/// whatever folder the user chose, Inbox or not. The folder is read off the
/// GTK thread (`spawn_cache_read`), and the rules are re-snapshotted when the
/// read lands, since they, the folders or the account may change meanwhile.
pub(crate) fn run_rules_now(state: &Rc<RefCell<UiState>>, worker: &crate::worker::Worker, account_id: &AccountId, mailbox: &MailboxId) {
    if !rules_enabled(state) {
        return;
    }
    if RunContext::for_account(state, account_id).is_none_or(|run| run.rules.is_empty()) {
        return;
    }
    let Some(cache) = state.borrow().accounts.get(account_id).and_then(|handle| handle.address_cache.clone()) else {
        return;
    };
    let reply_rx = crate::window::spawn_cache_read(worker, cache, {
        let mailbox = mailbox.clone();
        move |cache| cache.load_messages(&mailbox).unwrap_or_default()
    });
    let state = state.clone();
    let account_id = account_id.clone();
    gtk::glib::spawn_future_local(async move {
        let Ok(messages) = reply_rx.recv().await else { return };
        if messages.is_empty() || !rules_enabled(&state) {
            return;
        }
        let Some(run) = RunContext::for_account(&state, &account_id) else {
            return;
        };
        if !run.rules.is_empty() {
            evaluate_batch(&state, &run, &messages);
        }
    });
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

/// A forward or redirect a plan asks its caller to perform. The caller fetches
/// the message's raw bytes (interactively) and then sends it, because that is
/// I/O the pure planner can't do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedForward {
    pub to: Vec<EmailAddress>,
    pub kind: RuleForwardKind,
}

/// The commands an action chain produces for one message, plus what reverses
/// them and any forwards to queue. Pure: it neither sends nor touches UI
/// state, so the rules engine (which sends and forgets) and Quick Steps (which
/// sends, updates the list, and records a single Undo entry) share it (item
/// 2.5 → 2.6).
#[derive(Debug, Default)]
pub(crate) struct ActionPlan {
    /// Commands to send, in the chain's order.
    pub commands: Vec<AccountCommand>,
    /// Commands that reverse `commands` - for Quick Steps' undo. The rules
    /// engine ignores this.
    pub inverse: Vec<AccountCommand>,
    pub forwards: Vec<PlannedForward>,
    /// The chain ended with "stop processing more rules".
    pub stop: bool,
}

/// Translates one message's action chain into commands, inverse commands and
/// forwards. `folders` resolves move/copy targets and the Deleted Items
/// destination; `categories` resolves category names to their `$Lookout-tag-*`
/// keywords. An action whose target doesn't exist on this account is skipped
/// (never an error), and a move to the message's current folder is a no-op -
/// the same rules (no pun intended) the old inline dispatch followed.
pub(crate) fn plan_actions(folders: &[Mailbox], categories: &crate::categories::CategorySet, message: &EmailSummary, actions: &[RuleAction]) -> ActionPlan {
    let mailbox = &message.mailbox;
    let uid = message.uid;
    let mut plan = ActionPlan::default();
    for action in actions {
        match action {
            RuleAction::Move(target) => {
                if let Some(target) = resolve_target(folders, target) {
                    if target != *mailbox {
                        plan.commands.push(AccountCommand::MoveMessagesTo {
                            mailbox: mailbox.clone(),
                            uids: vec![uid],
                            target: target.clone(),
                        });
                        plan.inverse.push(AccountCommand::RevertMove {
                            moved_from: mailbox.clone(),
                            moved_to: target,
                            uids: vec![uid],
                        });
                    }
                }
            }
            RuleAction::Copy(target) => {
                if let Some(target) = resolve_target(folders, target) {
                    plan.commands.push(AccountCommand::CopyMessagesTo {
                        mailbox: mailbox.clone(),
                        uids: vec![uid],
                        target: target.clone(),
                    });
                    plan.inverse.push(AccountCommand::RevertCopy {
                        copied_from: mailbox.clone(),
                        copied_to: target,
                        uids: vec![uid],
                    });
                }
            }
            RuleAction::Delete => {
                plan.commands.push(AccountCommand::MoveMessage {
                    mailbox: mailbox.clone(),
                    uid,
                    role: MailboxRole::Trash,
                });
                if let Some(destination) = folders.iter().find(|folder| folder.role == MailboxRole::Trash).map(|folder| folder.id.clone()) {
                    plan.inverse.push(AccountCommand::RevertMove {
                        moved_from: mailbox.clone(),
                        moved_to: destination,
                        uids: vec![uid],
                    });
                }
            }
            RuleAction::Flag(flag) => {
                let before = message.follow_up().unwrap_or_default();
                plan.commands.push(AccountCommand::SetFollowUp {
                    mailbox: mailbox.clone(),
                    uids: vec![uid],
                    flag: flag.clone(),
                });
                plan.inverse.push(AccountCommand::SetFollowUp {
                    mailbox: mailbox.clone(),
                    uids: vec![uid],
                    flag: before,
                });
            }
            RuleAction::Categorize(names) => {
                let keywords: Vec<String> = names.iter().filter_map(|name| categories.find_by_name(name).map(|category| category.keyword())).collect();
                if !keywords.is_empty() {
                    plan.commands.push(AccountCommand::StoreKeywordsMany {
                        mailbox: mailbox.clone(),
                        uids: vec![uid],
                        add: keywords.clone(),
                        remove: Vec::new(),
                    });
                    plan.inverse.push(AccountCommand::StoreKeywordsMany {
                        mailbox: mailbox.clone(),
                        uids: vec![uid],
                        add: Vec::new(),
                        remove: keywords,
                    });
                }
            }
            RuleAction::MarkRead(read) => {
                let (add, remove) = if *read {
                    (vec![SystemFlagBit::Seen], Vec::new())
                } else {
                    (Vec::new(), vec![SystemFlagBit::Seen])
                };
                plan.commands.push(AccountCommand::StoreFlagsMany {
                    mailbox: mailbox.clone(),
                    uids: vec![uid],
                    add: add.clone(),
                    remove: remove.clone(),
                });
                plan.inverse.push(AccountCommand::StoreFlagsMany {
                    mailbox: mailbox.clone(),
                    uids: vec![uid],
                    add: remove,
                    remove: add,
                });
            }
            RuleAction::Forward(to) => {
                if !to.is_empty() {
                    plan.forwards.push(PlannedForward {
                        to: to.clone(),
                        kind: RuleForwardKind::Forward,
                    });
                }
            }
            RuleAction::Redirect(to) => {
                if !to.is_empty() {
                    plan.forwards.push(PlannedForward {
                        to: to.clone(),
                        kind: RuleForwardKind::Redirect,
                    });
                }
            }
            RuleAction::StopProcessing => {
                plan.stop = true;
                break;
            }
        }
    }
    plan
}

/// Runs the enabled rules over one message (with the fetched body when the
/// deferred path supplies one), executing the matching rules' actions in
/// order and honouring `StopProcessing`. The action chain itself is built by
/// [`plan_actions`], so a rule and a Quick Step translate an action list
/// identically.
fn evaluate_message(state: &Rc<RefCell<UiState>>, run: &RunContext, message: &EmailSummary, body: Option<&EmailBody>) {
    let fallback_headers: &[(String, String)] = &[];
    let context = RuleContext {
        body: body.and_then(|body| body.text_body.as_deref()),
        headers: body.map(|body| body.headers.as_slice()).unwrap_or(fallback_headers),
        owner: &run.owner,
        date: None,
    };
    for rule in &run.rules {
        if !rule.matches_for_account(&run.account_id, message, &context) {
            continue;
        }
        let plan = plan_actions(&run.folders, &run.categories, message, &rule.actions);
        for command in plan.commands {
            let _ = run.cmd_tx.send_blocking(command);
        }
        for forward in plan.forwards {
            queue_forward(state, &run.interactive_cmd_tx, &run.account_email, message, &forward.to, forward.kind);
        }
        if plan.stop {
            return;
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

/// Queues a rule or quick-step forward/redirect and asks the session for the
/// message's raw bytes; `deliver_rule_forwards` sends it once they arrive.
/// Messages the account itself sent are skipped, so a forward-to-self rule
/// can't loop. Shared by the rules engine and Quick Steps (item 2.6).
pub(crate) fn queue_forward(
    state: &Rc<RefCell<UiState>>,
    interactive_cmd_tx: &async_channel::Sender<AccountCommand>,
    account_email: &str,
    message: &EmailSummary,
    to: &[EmailAddress],
    kind: RuleForwardKind,
) {
    if to.is_empty() {
        return;
    }
    if message.from.iter().any(|address| address.address.eq_ignore_ascii_case(account_email)) {
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
    let _ = interactive_cmd_tx.send_blocking(AccountCommand::FetchRawMessage {
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
            bytes: std::sync::Arc::from(raw),
        }],
        inline_images: Vec::new(),
        calendar_part: None,
        read_receipt: None,
        request_read_receipt: false,
        request_delivery_receipt: false,
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

    fn summary() -> EmailSummary {
        EmailSummary {
            inference_classification: Default::default(),
            uid: Uid(7),
            mailbox: MailboxId("acct:INBOX".into()),
            message_id: Some("<m@example.com>".into()),
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey("m@example.com".into()),
            subject: Some("Hi".into()),
            from: vec![EmailAddress::new("alice@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            date: "2026-10-14T09:00:00Z".parse().unwrap(),
            flags: std::collections::BTreeSet::new(),
            keywords: std::collections::BTreeSet::new(),
            size: 1024,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            mentions: false,
            importance: lookout_core::Importance::Normal,
            sensitivity: lookout_core::MessageSensitivity::Normal,
            list_id: None,
            structure: None,
        }
    }

    fn test_categories() -> crate::categories::CategorySet {
        crate::categories::CategorySet::from_defs(vec![crate::categories::CategoryDef::new("news", "Newsletters", 7)])
    }

    #[test]
    fn plans_move_and_delete_with_inverses() {
        let folders = vec![
            folder("INBOX", "Inbox", MailboxRole::Inbox),
            folder("Archive", "Archive", MailboxRole::Archive),
            folder("Trash", "Deleted Items", MailboxRole::Trash),
        ];
        let message = summary();
        let plan = plan_actions(
            &folders,
            &test_categories(),
            &message,
            &[RuleAction::Move(RuleTarget::Role(MailboxRole::Archive)), RuleAction::Delete],
        );
        assert_eq!(plan.commands.len(), 2);
        assert!(matches!(&plan.commands[0], AccountCommand::MoveMessagesTo { target, .. } if target.0 == "acct:Archive"));
        assert!(matches!(&plan.commands[1], AccountCommand::MoveMessage { role: MailboxRole::Trash, .. }));
        assert_eq!(plan.inverse.len(), 2);
        assert!(matches!(&plan.inverse[0], AccountCommand::RevertMove { moved_to, .. } if moved_to.0 == "acct:Archive"));
        assert!(matches!(&plan.inverse[1], AccountCommand::RevertMove { moved_to, .. } if moved_to.0 == "acct:Trash"));
        assert!(!plan.stop && plan.forwards.is_empty());
    }

    #[test]
    fn plans_copy_read_flag_and_categorize_with_restoring_inverses() {
        let folders = vec![folder("INBOX", "Inbox", MailboxRole::Inbox), folder("Archive", "Archive", MailboxRole::Archive)];
        let message = summary();
        let flag = lookout_core::FollowUpFlag {
            status: lookout_core::FlagStatus::Flagged,
            ..Default::default()
        };
        let plan = plan_actions(
            &folders,
            &test_categories(),
            &message,
            &[
                RuleAction::Copy(RuleTarget::Role(MailboxRole::Archive)),
                RuleAction::MarkRead(true),
                RuleAction::Flag(flag),
                RuleAction::Categorize(vec!["Newsletters".into()]),
            ],
        );
        assert_eq!(plan.commands.len(), 4);
        assert_eq!(plan.inverse.len(), 4);
        assert!(matches!(&plan.inverse[0], AccountCommand::RevertCopy { copied_to, .. } if copied_to.0 == "acct:Archive"));
        // Mark-read's inverse marks unread again.
        assert!(matches!(&plan.inverse[1], AccountCommand::StoreFlagsMany { add, remove, .. } if add.is_empty() && remove == &vec![SystemFlagBit::Seen]));
        // The flag's inverse restores the (unset) previous state.
        assert!(matches!(&plan.inverse[2], AccountCommand::SetFollowUp { flag, .. } if flag.status == lookout_core::FlagStatus::NotFlagged));
        // The categorize inverse removes the keyword it added.
        assert!(matches!(&plan.inverse[3], AccountCommand::StoreKeywordsMany { add, remove, .. } if add.is_empty() && !remove.is_empty()));
    }

    #[test]
    fn skips_missing_targets_current_folder_moves_and_unknown_categories() {
        let folders = vec![folder("INBOX", "Inbox", MailboxRole::Inbox)];
        let message = summary();
        // No Archive folder -> the move is skipped, not an error.
        let plan = plan_actions(&folders, &test_categories(), &message, &[RuleAction::Move(RuleTarget::Role(MailboxRole::Archive))]);
        assert!(plan.commands.is_empty() && plan.inverse.is_empty());
        // Moving to the folder it's already in is a no-op.
        let plan = plan_actions(&folders, &test_categories(), &message, &[RuleAction::Move(RuleTarget::Role(MailboxRole::Inbox))]);
        assert!(plan.commands.is_empty());
        // An unknown category name contributes no keyword.
        let plan = plan_actions(&folders, &test_categories(), &message, &[RuleAction::Categorize(vec!["Nope".into()])]);
        assert!(plan.commands.is_empty());
    }

    #[test]
    fn stop_processing_ends_the_chain_and_forwards_are_planned() {
        let folders = vec![folder("INBOX", "Inbox", MailboxRole::Inbox)];
        let message = summary();
        let plan = plan_actions(
            &folders,
            &test_categories(),
            &message,
            &[RuleAction::MarkRead(true), RuleAction::StopProcessing, RuleAction::Delete],
        );
        assert!(plan.stop);
        assert_eq!(plan.commands.len(), 1, "actions after StopProcessing never run");

        let plan = plan_actions(&folders, &test_categories(), &message, &[RuleAction::Forward(vec![EmailAddress::new("a@example.com")])]);
        assert!(plan.commands.is_empty());
        assert_eq!(plan.forwards.len(), 1);
        assert_eq!(plan.forwards[0].kind, RuleForwardKind::Forward);
        assert_eq!(plan.forwards[0].to, vec![EmailAddress::new("a@example.com")]);
    }
}
