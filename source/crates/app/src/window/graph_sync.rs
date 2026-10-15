//! Microsoft Graph sync glue: follow-up flags, categories, rules and automatic replies.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use lookout_core::{AccountId, EmailSummary};
use lookout_mail::session::AccountCommand;

use crate::message_list::MessageListModel;

use super::*;

/// How often Outlook's follow-up flags are re-read through Graph on
/// Microsoft 365 accounts (item 1.16) - the snooze rescan's cadence.
pub(crate) const GRAPH_FLAG_RESYNC: std::time::Duration = std::time::Duration::from_secs(15 * 60);

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

/// Mirrors a stored flag change to Outlook through Graph (item 1.16) on a
/// Microsoft 365 account that has granted Graph access. Best effort: a
/// failure is logged, and the next Graph snapshot reconciles the two.
pub(crate) fn mirror_follow_up_to_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId, message_ids: Vec<String>, flag: lookout_core::FollowUpFlag) {
    let Some(client) = state.borrow().accounts.get(account_id).and_then(|h| h.graph_mail.clone()) else {
        return;
    };
    if !client.consented() {
        return;
    }
    let zone = crate::calendar_tz::display_zone();
    glib::spawn_future_local(async move {
        for message_id in message_ids {
            if let Err(e) = client.set_follow_up(&message_id, &flag, zone).await {
                tracing::warn!("Outlook flag mirror failed for {message_id}: {e}");
            }
        }
    });
}

/// Fetches the Graph snapshot of every flagged message on a Microsoft 365
/// account and hands it to the session (item 1.16), so flags set in Outlook
/// show here. A no-op for other accounts and before Graph access is granted.
pub(crate) fn sync_follow_ups_from_graph(state: &Rc<RefCell<UiState>>, account_id: &AccountId) {
    let target = state
        .borrow()
        .accounts
        .get(account_id)
        .and_then(|h| h.graph_mail.clone().map(|client| (client, h.cmd_tx.clone())));
    let Some((client, cmd_tx)) = target else {
        return;
    };
    if !client.consented() {
        return;
    }
    let zone = crate::calendar_tz::display_zone();
    glib::spawn_future_local(async move {
        let taken_at = chrono::Utc::now();
        match client.flagged_snapshot(zone).await {
            Ok(snapshot) => {
                let _ = cmd_tx.send(AccountCommand::ApplyRemoteFollowUps { snapshot, taken_at }).await;
            }
            Err(e) => tracing::warn!("couldn't read Outlook's flags: {e}"),
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
                // ones that need no extra widgets (flags, rules) and fetch
                // the automatic replies the dialog is waiting on.
                sync_follow_ups_from_graph(&state, &account_id);
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
