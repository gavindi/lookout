//! Microsoft Graph sync glue: follow-up flags, categories, rules and automatic replies.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::glib;
use lookout_core::{AccountId, EmailAddress, EmailSummary, Importance, InferenceClassification};
use lookout_mail::session::AccountCommand;

use crate::message_list::MessageListModel;

use super::*;

/// How often Outlook's per-message properties are re-read through Graph on
/// Microsoft 365 accounts (item 5.4) - the snooze rescan's cadence.
pub(crate) const GRAPH_PROPERTY_RESYNC: std::time::Duration = std::time::Duration::from_secs(15 * 60);

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
    let zone = display_zone_tz();
    let categories = state.borrow().categories.clone();
    glib::spawn_future_local(async move {
        let cursor = read_cache.as_ref().and_then(|cache| cache.graph_delta_cursor().ok().flatten());
        let taken_at = chrono::Utc::now();
        let delta = match client.property_delta(cursor.as_deref(), zone).await {
            Ok(delta) => delta,
            Err(e) => {
                tracing::warn!("couldn't read Outlook's mail properties: {e}");
                return;
            }
        };
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

/// Pulls a Microsoft 365 account's Outlook inbox rules into `AppConfig::rules`
/// (item 2.5 / M-14), adding any rule the user doesn't have locally yet (by
/// Graph id). A no-op for other accounts and before Graph access is granted.
/// Best effort: a failure is logged and the next sync reconciles.
pub(crate) fn sync_rules_from_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|h| h.graph_rules.clone()) else {
        return;
    };
    if !client.consented() {
        return;
    }
    let state = state.clone();
    glib::spawn_future_local(async move {
        match client.list().await {
            Ok(remote) => {
                let app_config = state.borrow().app_config.clone();
                let mut changed = false;
                {
                    let mut config = app_config.borrow_mut();
                    for rule in remote {
                        match config.rules.iter_mut().find(|r| r.id == rule.id) {
                            Some(existing) => {
                                if *existing != rule {
                                    *existing = rule;
                                    changed = true;
                                }
                            }
                            None => {
                                config.rules.push(rule);
                                changed = true;
                            }
                        }
                    }
                }
                if changed {
                    crate::app_config::save(&app_config.borrow());
                }
            }
            Err(e) => tracing::warn!("couldn't read Outlook's rules: {e}"),
        }
    });
}

/// Pushes every local `Graph`-backend rule to its Microsoft 365 account
/// (item 2.5 / M-14): an existing Outlook rule is updated in place, a new one
/// is created and its Graph id written back. A rule Outlook can't express
/// (see `graph_rules`) is left alone as a client rule. Best effort.
pub(crate) fn push_rules_to_graph(state: &Rc<RefCell<UiState>>) {
    let clients: Vec<(AccountId, Rc<crate::graph_rules::GraphRulesClient>)> = {
        let st = state.borrow();
        st.accounts
            .iter()
            .filter_map(|(account_id, handle)| handle.graph_rules.clone().filter(|client| client.consented()).map(|client| (account_id.clone(), client)))
            .collect()
    };
    if clients.is_empty() {
        return;
    }
    let state = state.clone();
    glib::spawn_future_local(async move {
        for (account_id, client) in clients {
            let local: Vec<lookout_core::Rule> = {
                let st = state.borrow();
                let rules = st
                    .app_config
                    .borrow()
                    .rules
                    .iter()
                    .filter(|rule| rule.backend == lookout_core::RuleBackend::Graph && rule.account.as_ref() == Some(&account_id))
                    .cloned()
                    .collect();
                rules
            };
            if local.is_empty() {
                continue;
            }
            let remote = match client.list().await {
                Ok(remote) => remote,
                Err(e) => {
                    tracing::warn!("couldn't read Outlook's rules before pushing: {e}");
                    continue;
                }
            };
            let remote_ids: Vec<String> = remote.iter().map(|rule| rule.id.clone()).collect();
            for rule in local {
                if remote_ids.contains(&rule.id) {
                    if let Err(e) = client.update(&rule).await {
                        tracing::warn!("couldn't update Outlook rule '{}': {e}", rule.name);
                    }
                } else {
                    match client.create(&rule).await {
                        Ok(new_id) if !new_id.is_empty() => {
                            let app_config = state.borrow().app_config.clone();
                            if let Some(stored) = app_config
                                .borrow_mut()
                                .rules
                                .iter_mut()
                                .find(|r| r.id == rule.id && r.account.as_ref() == Some(&account_id))
                            {
                                stored.id = new_id;
                            }
                            crate::app_config::save(&app_config.borrow());
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("couldn't create Outlook rule '{}': {e}", rule.name),
                    }
                }
            }
        }
    });
}

/// Pulls then pushes a Microsoft 365 account's Graph rules (item 2.5),
/// reconciling the two directions.
pub(crate) fn sync_and_push_rules_from_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    sync_rules_from_graph(state, account_id);
    push_rules_to_graph(state);
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
}
