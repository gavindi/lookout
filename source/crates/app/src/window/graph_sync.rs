//! Microsoft Graph sync glue: follow-up flags, categories, rules and automatic replies.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk::glib;
use lookout_core::{AccountId, EmailAddress, EmailSummary, Importance, InferenceClassification, Rule, RuleBackend, RuleSync};
use lookout_mail::session::AccountCommand;

use crate::message_list::MessageListModel;

use super::*;

/// How often Outlook's per-message properties are re-read through Graph on
/// Microsoft 365 accounts (item 5.4) - the snooze rescan's cadence.
pub(crate) const GRAPH_PROPERTY_RESYNC: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Per-account bookkeeping for `sync_mail_properties_from_graph`: which
/// accounts have a pull in flight, and the newest delta cursor this process
/// has received. The connect timer, the periodic resync, F5 and the consent
/// paths all trigger a pull, so without the in-flight set they overlap and
/// fetch the same page twice. The session persists the same cursor, but only
/// once it processes `ApplyRemoteMailProperties` - a pull started in between
/// would read the stale one from the cache, so the in-memory copy wins.
#[derive(Default)]
pub(crate) struct GraphPropertySyncs {
    in_flight: HashSet<AccountId>,
    cursors: HashMap<AccountId, String>,
}

impl GraphPropertySyncs {
    /// `None` when a pull is already running for `id` (the caller skips);
    /// otherwise marks it running and returns the newest known cursor, if any.
    pub(crate) fn try_begin(&mut self, id: &AccountId) -> Option<Option<String>> {
        if !self.in_flight.insert(id.clone()) {
            return None;
        }
        Some(self.cursors.get(id).cloned())
    }

    pub(crate) fn record_cursor(&mut self, id: &AccountId, cursor: String) {
        self.cursors.insert(id.clone(), cursor);
    }

    pub(crate) fn finish(&mut self, id: &AccountId) {
        self.in_flight.remove(id);
    }

    /// Drops all state for a removed account - its cache, and the cursor in
    /// it, is deleted too, so a re-added account must start over.
    pub(crate) fn forget(&mut self, id: &AccountId) {
        self.in_flight.remove(id);
        self.cursors.remove(id);
    }
}

/// Releases an account's `GraphPropertySyncs` in-flight mark when a pull
/// ends, on every path out of it (including an early return on error).
struct PropertySyncGuard {
    state: Rc<RefCell<UiState>>,
    account_id: AccountId,
}

impl Drop for PropertySyncGuard {
    fn drop(&mut self) {
        self.state.borrow_mut().graph_property_syncs.finish(&self.account_id);
    }
}

/// Every account's flagged mail, merged - what the Tasks view lists.
pub(crate) fn all_flagged_mail(state: &Rc<RefCell<UiState>>) -> Vec<EmailSummary> {
    state.borrow().flagged_mail.values().flatten().cloned().collect()
}

/// Runs the flagged-mail repaint hook, if the window has registered one.
pub(crate) fn refresh_flagged_mail(state: &Rc<RefCell<UiState>>) {
    let hook = state.borrow().flagged_mail_refresh.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// The Graph mail-property client for `account_id`, when the account is
/// Microsoft 365 and has granted Graph access (item 5.4).
fn consented_graph_mail(state: &UiState, account_id: &AccountId) -> Option<Rc<crate::graph_mail::GraphMailClient>> {
    state
        .accounts
        .get(account_id)
        .and_then(|handle| handle.graph_mail.clone())
        .filter(|client| client.consented())
}

/// The zone a flag's due date is written in (item 1.16 / item 5.4).
fn display_zone_tz() -> Option<chrono_tz::Tz> {
    crate::calendar_tz::display_zone().tz()
}

/// Mirrors a stored flag change to Outlook through Graph (item 1.16 / item
/// 5.4). Best effort: a failure is logged, and the next delta pull reconciles
/// the two.
pub(crate) fn mirror_follow_up_to_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId, message_ids: Vec<String>, flag: lookout_core::FollowUpFlag) {
    let Some(client) = consented_graph_mail(&state.borrow(), account_id) else {
        return;
    };
    let zone = display_zone_tz();
    glib::spawn_future_local(async move {
        for message_id in message_ids {
            match client.resolve_message_id(&message_id).await {
                Ok(Some(graph_id)) => {
                    if let Err(e) = client.set_follow_up(&graph_id, &flag, zone).await {
                        tracing::warn!("Outlook flag mirror failed for {message_id}: {e}");
                    }
                }
                Ok(None) => tracing::debug!("no Graph message for {message_id}; skipping the flag mirror"),
                Err(e) => tracing::warn!("Outlook flag mirror lookup failed for {message_id}: {e}"),
            }
        }
    });
}

/// Mirrors a stored category change to Outlook through Graph (item 5.4 /
/// M-09): `updates` pairs each message's `Message-ID` with its resulting
/// Lookout category *keys*, which are mapped to Outlook's display names and
/// written as the message's whole category list.
pub(crate) fn mirror_categories_to_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId, updates: Vec<(String, Vec<String>)>) {
    if updates.is_empty() {
        return;
    }
    let Some(client) = consented_graph_mail(&state.borrow(), account_id) else {
        return;
    };
    let names_by_key: HashMap<String, String> = state
        .borrow()
        .categories
        .borrow()
        .categories
        .iter()
        .map(|def| (def.key.clone(), def.name.clone()))
        .collect();
    glib::spawn_future_local(async move {
        for (message_id, keys) in updates {
            let names: Vec<String> = keys.iter().filter_map(|key| names_by_key.get(key).cloned()).collect();
            match client.resolve_message_id(&message_id).await {
                Ok(Some(graph_id)) => {
                    if let Err(e) = client.set_categories(&graph_id, &names).await {
                        tracing::warn!("Outlook category mirror failed for {message_id}: {e}");
                    }
                }
                Ok(None) => tracing::debug!("no Graph message for {message_id}; skipping the category mirror"),
                Err(e) => tracing::warn!("Outlook category mirror lookup failed for {message_id}: {e}"),
            }
        }
    });
}

/// Mirrors an importance change to Outlook through Graph (item 5.4 / M-12).
pub(crate) fn mirror_importance_to_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId, message_ids: Vec<String>, importance: Importance) {
    let Some(client) = consented_graph_mail(&state.borrow(), account_id) else {
        return;
    };
    glib::spawn_future_local(async move {
        for message_id in message_ids {
            match client.resolve_message_id(&message_id).await {
                Ok(Some(graph_id)) => {
                    if let Err(e) = client.set_importance(&graph_id, importance).await {
                        tracing::warn!("Outlook importance mirror failed for {message_id}: {e}");
                    }
                }
                Ok(None) => tracing::debug!("no Graph message for {message_id}; skipping the importance mirror"),
                Err(e) => tracing::warn!("Outlook importance mirror lookup failed for {message_id}: {e}"),
            }
        }
    });
}

/// Mirrors a Focused/Other change to Outlook through Graph (item 5.4 / M-03):
/// each message's classification, and - for a "Always move…" change - a sender
/// override so future mail follows.
pub(crate) fn mirror_focused_to_graph(
    state: &Rc<RefCell<UiState>>,
    account_id: &AccountId,
    message_ids: Vec<String>,
    classification: InferenceClassification,
    sender: Option<EmailAddress>,
) {
    let Some(client) = consented_graph_mail(&state.borrow(), account_id) else {
        return;
    };
    glib::spawn_future_local(async move {
        for message_id in message_ids {
            match client.resolve_message_id(&message_id).await {
                Ok(Some(graph_id)) => {
                    if let Err(e) = client.set_inference_classification(&graph_id, classification).await {
                        tracing::warn!("Outlook Focused/Other mirror failed for {message_id}: {e}");
                    }
                }
                Ok(None) => tracing::debug!("no Graph message for {message_id}; skipping the Focused/Other mirror"),
                Err(e) => tracing::warn!("Outlook Focused/Other mirror lookup failed for {message_id}: {e}"),
            }
        }
        if let Some(sender) = sender {
            if let Err(e) = client.create_override(classification, &sender).await {
                tracing::warn!("Outlook Focused/Other override failed for {}: {e}", sender.address);
            }
        }
    });
}

/// Maps Outlook category names to Lookout keys through the shared master list
/// (item 2.1), deriving the same key `sanitize_tag_key` would for a name the
/// list doesn't have yet - so a category created in Outlook between master-list
/// pulls still lands under the key its later definition will use (item 5.4).
fn category_keys_for_names(set: &crate::categories::CategorySet, names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| set.find_by_name(name).map(|def| def.key.clone()).unwrap_or_else(|| lookout_core::sanitize_tag_key(name)))
        .collect()
}

/// Schedules `message` to be sent by Exchange at `at` on a Microsoft 365
/// account (item 5.5 / C-08): the message waits in Drafts and goes out even if
/// Lookout is closed. Returns `false` - doing nothing - when the account isn't
/// Microsoft 365 with Graph access, so the caller falls back to the local
/// outbox.
pub(crate) fn schedule_send_via_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId, message: &lookout_mail::ComposedMessage, at: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(client) = consented_graph_mail(&state.borrow(), account_id) else {
        return false;
    };
    let message = message.clone();
    let state = state.clone();
    let account_id = account_id.clone();
    glib::spawn_future_local(async move {
        match client.send_scheduled(&message, at).await {
            Ok(()) => {
                toast(
                    &state,
                    &format!(
                        "Scheduled for {} (sends even if Lookout is closed)",
                        crate::datetime_picker::format_short_time(&at.with_timezone(&chrono::Local), &chrono::Local::now())
                    ),
                );
                // Replace the IMAP-autosaved draft with the server-side one, so
                // Drafts doesn't end up holding two copies.
                if let Some(message_id) = message.message_id.clone() {
                    if let Some(cmd_tx) = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone()) {
                        let _ = cmd_tx.send_blocking(AccountCommand::DeleteDraft { message_id });
                    }
                }
            }
            Err(e) => toast(&state, &format!("Couldn't schedule the message: {e}")),
        }
    });
    true
}

/// Fetches one page of the Graph messages delta on a Microsoft 365 account and
/// hands it to the session (item 5.4), so categories, flags, importance and
/// Focused/Other set in Outlook show here. The cursor is read from (and the
/// next one stored in) the mail cache, so an incremental pull after the first.
/// A no-op for other accounts and before Graph access is granted.
pub(crate) fn sync_mail_properties_from_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let target = {
        let st = state.borrow();
        st.accounts.get(account_id).and_then(|handle| {
            handle
                .graph_mail
                .clone()
                .filter(|client| client.consented())
                .map(|client| (client, handle.cmd_tx.clone(), handle.address_cache.clone()))
        })
    };
    let Some((client, cmd_tx, read_cache)) = target else {
        return;
    };
    let Some(known_cursor) = state.borrow_mut().graph_property_syncs.try_begin(account_id) else {
        tracing::debug!(%account_id, "Outlook property sync already in flight, skipping");
        return;
    };
    let zone = display_zone_tz();
    let categories = state.borrow().categories.clone();
    let guard = PropertySyncGuard {
        state: state.clone(),
        account_id: account_id.clone(),
    };
    glib::spawn_future_local(async move {
        // The cache is only consulted until this process has seen a cursor
        // of its own, and then off the GTK thread: the read contends for the
        // cache connection with the session's write transactions.
        let cursor = match (known_cursor, read_cache) {
            (Some(cursor), _) => Some(cursor),
            (None, Some(cache)) => gtk::gio::spawn_blocking(move || cache.graph_delta_cursor().ok().flatten()).await.ok().flatten(),
            (None, None) => None,
        };
        let taken_at = chrono::Utc::now();
        let delta = match client.property_delta(cursor.as_deref(), zone).await {
            Ok(delta) => delta,
            Err(e) => {
                tracing::warn!("couldn't read Outlook's mail properties: {e}");
                return;
            }
        };
        // Recorded before the session has persisted it, so a pull starting
        // in the meantime doesn't refetch this page from the old cursor.
        if let Some(next) = delta.delta_link.clone() {
            guard.state.borrow_mut().graph_property_syncs.record_cursor(&guard.account_id, next);
        }
        let properties: Vec<lookout_mail::RemoteMailProperties> = delta
            .upserts
            .iter()
            .filter_map(|props| {
                let message_id = props.message_id.clone()?;
                // Outlook names -> Lookout keys (the master-list pull keeps the
                // definitions; an unknown name derives the same key it would).
                let keys = category_keys_for_names(&categories.borrow(), &props.categories);
                Some(lookout_mail::RemoteMailProperties {
                    message_id,
                    graph_id: props.graph_id.clone(),
                    categories: keys,
                    importance: props.importance,
                    classification: props.classification,
                    flag: props.flag.clone(),
                })
            })
            .collect();
        let _ = cmd_tx
            .send(AccountCommand::ApplyRemoteMailProperties {
                properties,
                removed_graph_ids: delta.removals,
                delta_cursor: delta.delta_link,
                taken_at,
            })
            .await;
        // The Focused/Other sender overrides (M-03), so "Stop moving" knows an
        // override's Graph id without a live lookup.
        if let Ok(overrides) = client.list_overrides().await {
            let rows: Vec<(String, InferenceClassification, Option<String>)> = overrides
                .into_iter()
                .map(|override_entry| (override_entry.sender.address, override_entry.classify_as, Some(override_entry.id)))
                .collect();
            let _ = cmd_tx.send(AccountCommand::ApplyInferenceOverrides { overrides: rows, taken_at }).await;
        }
    });
}

/// Pushes local categories that Outlook doesn't have yet to every connected,
/// consented Microsoft 365 account (item 2.1). Union semantics: a category is
/// created remotely when its name is missing there, and nothing is ever renamed
/// or deleted remotely here (so Outlook-only categories are never at risk).
/// Best effort - a failure is logged and the next sync reconciles.
pub(crate) fn push_categories_to_graph(state: &Rc<RefCell<UiState>>) {
    let clients: Vec<Rc<crate::graph_categories::GraphCategoriesClient>> = {
        let st = state.borrow();
        st.accounts
            .values()
            .filter_map(|h| h.graph_categories.clone())
            .filter(|client| client.consented())
            .collect()
    };
    if clients.is_empty() {
        return;
    }
    let set = state.borrow().categories.borrow().clone();
    glib::spawn_future_local(async move {
        for client in clients {
            match client.list().await {
                Ok(remote) => {
                    for cat in &set.categories {
                        match remote.iter().find(|r| r.name.eq_ignore_ascii_case(&cat.name)) {
                            // A name Outlook already has: only push a colour
                            // change, never rename or delete remotely.
                            Some(r) if r.preset != cat.preset => {
                                if let Err(e) = client.update(&r.id, &cat.name, cat.preset).await {
                                    tracing::warn!("couldn't update Outlook category '{}': {e}", cat.name);
                                }
                            }
                            Some(_) => {}
                            None => {
                                if let Err(e) = client.create(&cat.name, cat.preset).await {
                                    tracing::warn!("couldn't create Outlook category '{}': {e}", cat.name);
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!("couldn't read Outlook's categories: {e}"),
            }
        }
    });
}

/// Pulls a Microsoft 365 account's Outlook categories into the shared master
/// list (item 2.1), adding any name the user doesn't have locally yet. A no-op
/// for other accounts and before Graph access is granted. Union semantics: a
/// local category is never overwritten by the remote preset here (an existing
/// colour choice wins), and nothing local is removed.
pub(crate) fn sync_categories_from_graph(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, account_id: &AccountId) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|h| h.graph_categories.clone()) else {
        return;
    };
    if !client.consented() {
        return;
    }
    let state = state.clone();
    let message_list = message_list.clone();
    glib::spawn_future_local(async move {
        match client.list().await {
            Ok(remote) => {
                let tags = state.borrow().categories.clone();
                let mut changed = false;
                {
                    let mut set = tags.borrow_mut();
                    for r in remote {
                        if !set.contains_name(&r.name) {
                            let key = lookout_core::sanitize_tag_key(&r.name);
                            set.categories.push(crate::categories::CategoryDef::new(key, r.name, r.preset));
                            changed = true;
                        }
                    }
                }
                if changed {
                    save_categories(&state, &tags.borrow());
                    message_list.refresh();
                }
            }
            Err(e) => tracing::warn!("couldn't read Outlook's categories: {e}"),
        }
    });
}

/// Per-account bookkeeping for the Outlook rules sync: one pull+push in
/// flight per account. Without it, the connect timer, F5 and the consent
/// paths can each start a sync, and the push would create a rule again before
/// another sync has written its Graph id back.
#[derive(Default)]
pub(crate) struct GraphRuleSyncs {
    in_flight: HashSet<AccountId>,
}

impl GraphRuleSyncs {
    /// `false` when a sync is already running for `id` (the caller skips).
    pub(crate) fn try_begin(&mut self, id: &AccountId) -> bool {
        self.in_flight.insert(id.clone())
    }

    pub(crate) fn finish(&mut self, id: &AccountId) {
        self.in_flight.remove(id);
    }

    /// Drops the in-flight mark for a removed account.
    pub(crate) fn forget(&mut self, id: &AccountId) {
        self.in_flight.remove(id);
    }
}

/// Releases an account's `GraphRuleSyncs` mark when a sync ends, on every
/// path out of it (including an early return on error).
struct RuleSyncGuard {
    state: Rc<RefCell<UiState>>,
    account_id: AccountId,
}

impl Drop for RuleSyncGuard {
    fn drop(&mut self) {
        self.state.borrow_mut().graph_rule_syncs.finish(&self.account_id);
    }
}

/// What the reconciliation decided must go to Outlook.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RulePush {
    /// A rule this account has never accepted: create it remotely and write
    /// the returned Graph id back.
    Create(String),
    /// A rule with a pending local edit: update its remote copy.
    Update(String),
}

/// Rules created by the editor use a UUID
/// (`rules_view::new_rule_id`); Graph `messageRule` ids are opaque strings in
/// a different shape. Used only to migrate settings written before
/// `Rule::synced` existed: a non-UUID id found remotely was already pushed.
fn looks_like_local_rule_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}

/// Reconciles this account's local Graph rules with Outlook's list, in place.
///
/// Returns the remote writes to perform and whether `local` changed. The
/// rules:
///
/// * A rule whose local fingerprint still matches `synced.fingerprint` has no
///   pending edit, so Outlook's copy wins (it may have been edited there, or
///   normalised by Graph).
/// * A rule whose fingerprint differs has a local edit; it is kept and marked
///   for `Update`, so the next push wins instead of the pull overwriting it.
/// * A rule with a sync marker for this account whose id is missing remotely
///   was deleted in Outlook and is removed locally - it must not be re-created
///   by the next push.
/// * A rule with no marker for this account (new, or re-scoped from another
///   account) is `Create`d.
fn reconcile_rules(local: &mut Vec<Rule>, remote: Vec<Rule>, account: &AccountId) -> (Vec<RulePush>, bool) {
    let remote_ids: HashSet<String> = remote.iter().map(|rule| rule.id.clone()).collect();
    let mut remote_by_id: HashMap<String, Rule> = remote.into_iter().map(|rule| (rule.id.clone(), rule)).collect();
    let mut pushes = Vec::new();
    let mut doomed: HashSet<String> = HashSet::new();
    let mut changed = false;

    for rule in local.iter_mut().filter(|rule| rule.backend == RuleBackend::Graph && rule.account.as_ref() == Some(account)) {
        // Migration: an older build wrote the Graph id straight into `id` and
        // never stored a marker. A non-UUID id was therefore pushed at least
        // once: if it is still remote, treat the current content as the
        // baseline; if it is gone, Outlook deleted it. A UUID id is a local
        // rule the old push never completed, so it is created below.
        if rule.synced.is_none() && !looks_like_local_rule_id(&rule.id) {
            if remote_ids.contains(&rule.id) {
                rule.synced = Some(RuleSync {
                    account: account.clone(),
                    fingerprint: rule.sync_fingerprint(),
                });
                changed = true;
            } else {
                doomed.insert(rule.id.clone());
                continue;
            }
        }

        let Some(mut remote_rule) = remote_by_id.remove(&rule.id) else {
            if rule.synced.as_ref().is_some_and(|sync| &sync.account == account) {
                // It has a marker for this mailbox, but Outlook no longer has
                // it: deleted there while we were away. Remove it locally
                // instead of letting the push resurrect it.
                doomed.insert(rule.id.clone());
            } else {
                pushes.push(RulePush::Create(rule.id.clone()));
            }
            continue;
        };

        let unchanged = rule
            .synced
            .as_ref()
            .is_some_and(|sync| &sync.account == account && sync.fingerprint == rule.sync_fingerprint());
        if unchanged {
            remote_rule.synced = Some(RuleSync {
                account: account.clone(),
                fingerprint: remote_rule.sync_fingerprint(),
            });
            if *rule != remote_rule {
                *rule = remote_rule;
                changed = true;
            }
        } else {
            // A local edit is waiting: keep it; the push below writes it to
            // Outlook instead of the pull clobbering it.
            pushes.push(RulePush::Update(rule.id.clone()));
        }
    }

    // A remote rule no local rule matched is new to this account.
    for mut remote_rule in remote_by_id.into_values() {
        remote_rule.synced = Some(RuleSync {
            account: account.clone(),
            fingerprint: remote_rule.sync_fingerprint(),
        });
        local.push(remote_rule);
        changed = true;
    }

    if !doomed.is_empty() {
        local.retain(|rule| !(rule.backend == RuleBackend::Graph && rule.account.as_ref() == Some(account) && doomed.contains(&rule.id)));
        changed = true;
    }

    (pushes, changed)
}

/// Pulls then pushes a Microsoft 365 account's Graph rules (item 2.5),
/// reconciling the two directions for that account only. One future, one
/// in-flight slot: the pull and push cannot race each other, and two accounts
/// no longer push concurrently. A no-op for other accounts and before Graph
/// access is granted; best effort, so a failure is logged and the next sync
/// reconciles.
pub(crate) fn sync_and_push_rules_from_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|handle| handle.graph_rules.clone()) else {
        return;
    };
    if !client.consented() {
        return;
    }
    if !state.borrow_mut().graph_rule_syncs.try_begin(account_id) {
        tracing::debug!(%account_id, "Outlook rules sync already in flight, skipping");
        return;
    }
    let state = state.clone();
    let account_id = account_id.clone();
    glib::spawn_future_local(async move {
        let _guard = RuleSyncGuard {
            state: state.clone(),
            account_id: account_id.clone(),
        };
        let remote = match client.list().await {
            Ok(remote) => remote,
            Err(e) => {
                tracing::warn!("couldn't read Outlook's rules: {e}");
                return;
            }
        };

        // Reconcile in one short borrow, then release it before any await.
        let (pushes, changed) = {
            let app_config = state.borrow().app_config.clone();
            let mut config = app_config.borrow_mut();
            let (pushes, changed) = reconcile_rules(&mut config.rules, remote, &account_id);
            drop(config);
            if changed {
                crate::app_config::save(&app_config.borrow());
            }
            (pushes, changed)
        };
        if changed {
            tracing::debug!(%account_id, "reconciled Outlook rules with settings");
        }

        for push in pushes {
            let Some(rule) = state
                .borrow()
                .app_config
                .borrow()
                .rules
                .iter()
                .find(|rule| {
                    rule.account.as_ref() == Some(&account_id)
                        && rule.backend == RuleBackend::Graph
                        && match &push {
                            RulePush::Create(id) | RulePush::Update(id) => &rule.id == id,
                        }
                })
                .cloned()
            else {
                continue;
            };
            // Fingerprint of exactly what goes over the wire. If the rule is
            // edited again while the request is in flight, the stored content
            // no longer matches this and the next sync pushes the newer edit
            // instead of treating it as already synced.
            let sent_fingerprint = rule.sync_fingerprint();
            match push {
                RulePush::Create(local_id) => match client.create(&rule).await {
                    Ok(new_id) if !new_id.is_empty() => {
                        let app_config = state.borrow().app_config.clone();
                        let updated = {
                            let mut config = app_config.borrow_mut();
                            match config.rules.iter_mut().find(|stored| stored.id == local_id && stored.account.as_ref() == Some(&account_id)) {
                                Some(stored) => {
                                    stored.id = new_id;
                                    stored.synced = Some(RuleSync {
                                        account: account_id.clone(),
                                        fingerprint: sent_fingerprint,
                                    });
                                    true
                                }
                                None => false,
                            }
                        };
                        if updated {
                            crate::app_config::save(&app_config.borrow());
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("couldn't create Outlook rule '{}': {e}", rule.name),
                },
                RulePush::Update(local_id) => match client.update(&rule).await {
                    Ok(()) => {
                        let app_config = state.borrow().app_config.clone();
                        let updated = {
                            let mut config = app_config.borrow_mut();
                            match config.rules.iter_mut().find(|stored| stored.id == local_id && stored.account.as_ref() == Some(&account_id)) {
                                Some(stored) => {
                                    stored.synced = Some(RuleSync {
                                        account: account_id.clone(),
                                        fingerprint: sent_fingerprint,
                                    });
                                    true
                                }
                                None => false,
                            }
                        };
                        if updated {
                            crate::app_config::save(&app_config.borrow());
                        }
                    }
                    Err(e) => tracing::warn!("couldn't update Outlook rule '{}': {e}", rule.name),
                },
            }
        }
    });
}

/// Pulls a Microsoft 365 account's automatic replies from Graph (item 2.11 /
/// M-24) and repaints the Mail banner (and any open dialog) from the result.
/// Silent on failure - the banner simply stays as it was; the settings dialog
/// surfaces errors when it's open.
pub(crate) fn sync_automatic_replies_from_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|handle| handle.graph_automatic_replies.clone()) else {
        return;
    };
    if !client.consented() {
        return;
    }
    let state = state.clone();
    let account_id = account_id.clone();
    glib::spawn_future_local(async move {
        match client.get().await {
            Ok(settings) => {
                state.borrow_mut().automatic_replies.insert(account_id.clone(), settings);
                refresh_automatic_replies_ui(&state);
            }
            Err(e) => tracing::warn!("couldn't read automatic replies for {account_id}: {e}"),
        }
    });
}

/// Runs the interactive Graph consent flow for an account that hasn't opted
/// into Outlook sync yet (the Automatic replies dialog's "Enable Outlook
/// sync" action, item 2.11), then fetches the mailbox's replies.
pub(crate) fn enable_graph_automatic_replies(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|handle| handle.graph_automatic_replies.clone()) else {
        return;
    };
    let state = state.clone();
    let account_id = account_id.clone();
    glib::spawn_future_local(async move {
        match client.ensure_consent().await {
            Ok(()) => {
                toast(&state, "Outlook sync enabled");
                // The same grant covers the other Graph features; sync the
                // ones that need no extra widgets (properties, rules) and
                // fetch the automatic replies the dialog is waiting on.
                sync_mail_properties_from_graph(&state, &account_id);
                sync_and_push_rules_from_graph(&state, &account_id);
                sync_automatic_replies_from_graph(&state, &account_id);
            }
            Err(message) => toast(&state, &format!("Couldn't enable Outlook sync: {message}")),
        }
    });
}

/// Shows a toast through the shared overlay, best effort (absent in tests).
pub(crate) fn toast(state: &Rc<RefCell<UiState>>, message: &str) {
    let overlay = state.borrow().toast_overlay.clone();
    if let Some(overlay) = overlay {
        overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(message)));
    }
}

/// The automatic-reply state that applies right now, per connected account:
/// `(account_id, settings, graph client)`. Sieve accounts read their
/// `settings.json` entry; Microsoft 365 accounts read the last Graph fetch.
/// Accounts with replies off (or outside their schedule) are left out.
pub(crate) fn active_automatic_replies(
    state: &UiState,
) -> Vec<(
    AccountId,
    lookout_core::AutomaticReplies,
    Option<Rc<crate::graph_automatic_replies::GraphAutomaticRepliesClient>>,
)> {
    let now = chrono::Utc::now();
    let mut active = Vec::new();
    for (account_id, handle) in &state.accounts {
        let (settings, graph) = match &handle.graph_automatic_replies {
            Some(client) => (state.automatic_replies.get(account_id).cloned(), Some(client.clone())),
            // Sieve accounts read their stored settings; one whose server no
            // longer advertises ManageSieve isn't sending anything, so its
            // (stale) entry is ignored.
            None if handle.sieve_advertised => (state.app_config.borrow().automatic_replies_for(account_id), None),
            None => (None, None),
        };
        if let Some(settings) = settings {
            if settings.is_active_at(now) {
                active.push((account_id.clone(), settings, graph));
            }
        }
    }
    active.sort_by(|a, b| a.0.cmp(&b.0));
    active
}

/// The Mail banner's title (item 2.11), or `None` while no account has
/// automatic replies active.
pub(crate) fn automatic_replies_banner_text(state: &UiState) -> Option<String> {
    let active = active_automatic_replies(state);
    match active.len() {
        0 => None,
        1 => {
            let settings = &active[0].1;
            match (settings.status, settings.end) {
                (lookout_core::AutomaticReplyStatus::Scheduled, Some(end)) => Some(format!(
                    "Automatic replies are on until {}",
                    crate::datetime_picker::format_short_time(&end.with_timezone(&chrono::Local), &chrono::Local::now())
                )),
                _ => Some("Automatic replies are on".to_string()),
            }
        }
        count => Some(format!("Automatic replies are on for {count} accounts")),
    }
}

/// Reveals or hides the automatic-replies banner and repaints an open
/// Automatic replies dialog (item 2.11).
pub(crate) fn refresh_automatic_replies_ui(state: &Rc<RefCell<UiState>>) {
    let (banner, title, dialog_refresh) = {
        let st = state.borrow();
        (
            st.automatic_replies_banner.clone(),
            automatic_replies_banner_text(&st),
            st.automatic_replies_dialog_refresh.clone(),
        )
    };
    if let Some(banner) = banner {
        match title {
            Some(title) => {
                banner.set_title(&glib::markup_escape_text(&title));
                banner.set_revealed(true);
            }
            None => banner.set_revealed(false),
        }
    }
    if let Some(refresh) = dialog_refresh {
        refresh();
    }
}

/// The banner's "Turn off" action: disables automatic replies on every
/// account currently active them - Graph via a PATCH, Sieve via
/// `settings.json` plus a script re-push - and hides the banner.
pub(crate) fn turn_off_automatic_replies(state: &Rc<RefCell<UiState>>) {
    let active = active_automatic_replies(&state.borrow());
    if active.is_empty() {
        return;
    }
    let mut sieve_changed = false;
    for (account_id, settings, graph) in active {
        let disabled = lookout_core::AutomaticReplies {
            status: lookout_core::AutomaticReplyStatus::Disabled,
            ..settings
        };
        match graph {
            Some(client) => {
                let state = state.clone();
                glib::spawn_future_local(async move {
                    match client.set(&disabled).await {
                        Ok(()) => {
                            state.borrow_mut().automatic_replies.insert(account_id.clone(), disabled);
                            refresh_automatic_replies_ui(&state);
                        }
                        Err(e) => toast(&state, &format!("Couldn't turn off automatic replies: {e}")),
                    }
                });
            }
            None => {
                let app_config = state.borrow().app_config.clone();
                app_config.borrow_mut().upsert_automatic_replies(account_id, disabled);
                crate::app_config::save(&app_config.borrow());
                sieve_changed = true;
            }
        }
    }
    if sieve_changed {
        push_sieve_script(state);
    }
    refresh_automatic_replies_ui(state);
    toast(state, "Automatic replies turned off");
}

/// Compiles each ManageSieve account's Sieve script - its enabled
/// `Sieve`-backend rules plus its automatic replies (item 2.11 / M-24) - and
/// uploads it (item 2.5). A rule with no account applies to every such server;
/// a scoped rule and an account's automatic replies only to their own. Best
/// effort: the session reports success or failure with a toast.
pub(crate) fn push_sieve_script(state: &Rc<RefCell<UiState>>) {
    let targets: Vec<AccountId> = {
        let st = state.borrow();
        st.accounts
            .iter()
            .filter(|(_, handle)| handle.sieve_advertised)
            .map(|(account_id, _)| account_id.clone())
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let (rules, automatic_replies) = {
        let st = state.borrow();
        let app_config = st.app_config.borrow();
        (app_config.rules.clone(), app_config.automatic_replies.clone())
    };
    for account_id in targets {
        let scoped: Vec<lookout_core::Rule> = rules
            .iter()
            .filter(|rule| rule.account.as_ref().is_none_or(|account| account == &account_id))
            .cloned()
            .collect();
        let vacation = automatic_replies.iter().find(|entry| entry.account_id == account_id).map(|entry| entry.settings.clone());
        let script = lookout_mail::sieve::build_sieve_script(&scoped, vacation.as_ref());
        if let Some(handle) = state.borrow().accounts.get(&account_id) {
            let _ = handle.cmd_tx.send_blocking(AccountCommand::PutSieveScript { script });
        }
    }
}

/// Deletes a rule's Outlook copy (item 2.5) after the user removed it here.
/// A no-op for other accounts and before Graph access is granted; best effort.
pub(crate) fn delete_graph_rule(state: &Rc<RefCell<UiState>>, account_id: &AccountId, id: String) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|h| h.graph_rules.clone()) else {
        return;
    };
    if !client.consented() {
        return;
    }
    glib::spawn_future_local(async move {
        if let Err(e) = client.delete(&id).await {
            tracing::warn!("couldn't delete Outlook rule {id}: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outlook category names map to Lookout keys through the shared master
    /// list, and a name it doesn't know yet derives the key its later
    /// definition will use (item 5.4).
    #[test]
    fn category_keys_map_names_via_the_master_list_or_derive() {
        let set = crate::categories::CategorySet::from_defs(vec![crate::categories::CategoryDef::new("work-items", "Work Items", 3)]);
        // "work items" matches case-insensitively; "Personal" is unknown and
        // derives the sanitized key.
        assert_eq!(
            category_keys_for_names(&set, &["work items".to_string(), "Personal".to_string()]),
            vec!["work-items".to_string(), lookout_core::sanitize_tag_key("Personal")]
        );
        assert!(category_keys_for_names(&set, &[]).is_empty());
    }

    /// A second property pull for the same account is refused until the first
    /// finishes; other accounts are independent.
    #[test]
    fn property_sync_runs_once_per_account_at_a_time() {
        let a = AccountId("a".into());
        let b = AccountId("b".into());
        let mut syncs = GraphPropertySyncs::default();
        assert_eq!(syncs.try_begin(&a), Some(None));
        assert_eq!(syncs.try_begin(&a), None);
        assert_eq!(syncs.try_begin(&b), Some(None));
        syncs.finish(&a);
        assert_eq!(syncs.try_begin(&a), Some(None));
    }

    /// The next pull starts from the newest cursor this process received,
    /// even before the session has persisted it.
    #[test]
    fn property_sync_resumes_from_the_recorded_cursor() {
        let a = AccountId("a".into());
        let mut syncs = GraphPropertySyncs::default();
        assert!(syncs.try_begin(&a).is_some());
        syncs.record_cursor(&a, "c1".to_string());
        syncs.record_cursor(&a, "c2".to_string());
        syncs.finish(&a);
        assert_eq!(syncs.try_begin(&a), Some(Some("c2".to_string())));
    }

    /// Removing an account drops both its in-flight mark and its cursor.
    #[test]
    fn property_sync_forget_clears_the_account() {
        let a = AccountId("a".into());
        let mut syncs = GraphPropertySyncs::default();
        assert!(syncs.try_begin(&a).is_some());
        syncs.record_cursor(&a, "c1".to_string());
        syncs.forget(&a);
        assert_eq!(syncs.try_begin(&a), Some(None));
    }

    fn graph_rule(account: &AccountId, id: &str, name: &str) -> Rule {
        let mut rule = Rule::new(id, name);
        rule.backend = RuleBackend::Graph;
        rule.account = Some(account.clone());
        rule
    }

    /// Mark `rule` as already accepted by `account`, with a fingerprint of
    /// its current content.
    fn mark_synced(mut rule: Rule, account: &AccountId) -> Rule {
        rule.synced = Some(RuleSync {
            account: account.clone(),
            fingerprint: rule.sync_fingerprint(),
        });
        rule
    }

    #[test]
    fn rule_sync_runs_once_per_account_at_a_time() {
        let a = AccountId("a".into());
        let b = AccountId("b".into());
        let mut syncs = GraphRuleSyncs::default();
        assert!(syncs.try_begin(&a));
        assert!(!syncs.try_begin(&a));
        assert!(syncs.try_begin(&b));
        syncs.finish(&a);
        assert!(syncs.try_begin(&a));
        syncs.forget(&b);
        assert!(syncs.try_begin(&b));
    }

    #[test]
    fn a_rule_deleted_in_outlook_is_removed_locally_and_not_recreated() {
        let account = AccountId("a".into());
        let local = mark_synced(graph_rule(&account, "graph-rule-1", "Newsletters"), &account);
        let mut rules = vec![local];
        let (pushes, changed) = reconcile_rules(&mut rules, Vec::new(), &account);
        assert!(pushes.is_empty(), "a remotely deleted rule must not be created again: {pushes:?}");
        assert!(changed);
        assert!(rules.is_empty(), "the local copy should be gone");
    }

    #[test]
    fn an_unpushed_local_rule_is_created() {
        let account = AccountId("a".into());
        let id = uuid::Uuid::new_v4().to_string();
        let mut rules = vec![graph_rule(&account, &id, "New rule")];
        let (pushes, changed) = reconcile_rules(&mut rules, Vec::new(), &account);
        assert_eq!(pushes, vec![RulePush::Create(id.clone())]);
        assert!(!changed, "nothing about the local rule changed");
        assert_eq!(rules.len(), 1, "the rule stays until the push returns its id");
    }

    #[test]
    fn a_pending_local_edit_is_pushed_instead_of_overwritten() {
        let account = AccountId("a".into());
        let mut local = mark_synced(graph_rule(&account, "graph-rule-1", "Original"), &account);
        local.name = "Edited locally".to_string();
        let remote = graph_rule(&account, "graph-rule-1", "Changed in Outlook");
        let mut rules = vec![local];
        let (pushes, _) = reconcile_rules(&mut rules, vec![remote], &account);
        assert_eq!(pushes, vec![RulePush::Update("graph-rule-1".to_string())]);
        assert_eq!(rules[0].name, "Edited locally", "the pull must not clobber a pending edit");
    }

    #[test]
    fn an_unchanged_local_rule_adopts_outlooks_copy() {
        let account = AccountId("a".into());
        let local = mark_synced(graph_rule(&account, "graph-rule-1", "Original"), &account);
        let remote = graph_rule(&account, "graph-rule-1", "Changed in Outlook");
        let mut rules = vec![local];
        let (pushes, changed) = reconcile_rules(&mut rules, vec![remote], &account);
        assert!(pushes.is_empty());
        assert!(changed);
        assert_eq!(rules[0].name, "Changed in Outlook");
        assert!(rules[0].synced.is_some());
    }

    #[test]
    fn a_new_remote_rule_is_imported_once() {
        let account = AccountId("a".into());
        let mut rules = Vec::new();
        let (pushes, changed) = reconcile_rules(&mut rules, vec![graph_rule(&account, "graph-rule-1", "From Outlook")], &account);
        assert!(pushes.is_empty());
        assert!(changed);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "graph-rule-1");
        assert!(rules[0].synced.is_some());
    }

    #[test]
    fn a_legacy_rule_id_is_migrated_to_a_sync_marker() {
        let account = AccountId("a".into());
        // An older build stored the Graph id in `id` and no marker: present
        // remotely means pushed, so a later remote edit is adopted.
        let local = graph_rule(&account, "graph-rule-1", "Original");
        let remote = graph_rule(&account, "graph-rule-1", "Changed in Outlook");
        let mut rules = vec![local];
        let (pushes, changed) = reconcile_rules(&mut rules, vec![remote], &account);
        assert!(pushes.is_empty());
        assert!(changed);
        assert_eq!(rules[0].name, "Changed in Outlook");
        assert!(rules[0].synced.as_ref().is_some_and(|sync| sync.account == account));
    }

    #[test]
    fn a_legacy_rule_deleted_remotely_is_not_resurrected() {
        let account = AccountId("a".into());
        let mut rules = vec![graph_rule(&account, "graph-rule-1", "Original")];
        let (pushes, _) = reconcile_rules(&mut rules, Vec::new(), &account);
        // With no marker there is no way to tell "never pushed" from "pushed
        // by an old build": the id shape is the migration heuristic. A
        // non-UUID id absent remotely is treated as remote-deleted.
        assert!(pushes.is_empty());
        assert!(rules.is_empty());
    }

    #[test]
    fn a_rule_re_scoped_to_another_account_is_created_there() {
        let account = AccountId("b".into());
        let old_account = AccountId("a".into());
        let mut rule = mark_synced(graph_rule(&old_account, "graph-rule-1", "Newsletters"), &old_account);
        rule.account = Some(account.clone());
        let mut rules = vec![rule];
        let (pushes, _) = reconcile_rules(&mut rules, Vec::new(), &account);
        assert_eq!(pushes, vec![RulePush::Create("graph-rule-1".to_string())]);
        assert_eq!(rules.len(), 1, "the rule is not deleted just because it left the old account");
    }
}
