//! Folder-tree building, expansion/favorite persistence, and the folder menu handler.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{display_name, AccountId, Mailbox, MailboxId, MailboxRole};
use lookout_mail::session::AccountCommand;

use crate::folder_tree::{build_multi_account_tree_model, TreeItem};
use crate::last_view::{self, LastSelection};

use super::*;

pub(crate) fn folder_tree_signature(
    accounts: &[(AccountId, String, Vec<Mailbox>)],
    favorites: &[Mailbox],
    prefs: &HashMap<MailboxId, crate::ui_state_db::FolderPrefs>,
) -> FolderTreeSignature {
    let row = |m: &Mailbox| {
        let pref = prefs.get(&m.id).cloned().unwrap_or_default();
        (m.id.clone(), m.name.clone(), m.role, m.delimiter, m.unread, m.total, pref)
    };
    let mut signature: FolderTreeSignature = accounts
        .iter()
        .map(|(id, label, folders)| (id.clone(), label.clone(), folders.iter().map(row).collect()))
        .collect();
    // The favorites section is a real part of the rendered tree (it appears
    // and disappears with its membership), so it belongs in the signature.
    // Carried under a reserved pseudo-account id rather than a separate field
    // so the comparison stays a single `==`.
    signature.push((AccountId(FAVORITES_GROUP_KEY.into()), String::new(), favorites.iter().map(row).collect()));
    signature
}

/// Repaints the two unread-count indicators - the Ubuntu-dock badge (the
/// Unity LauncherEntry count) and the tray icon (the StatusNotifierItem
/// count) - with the summed Inbox unread count across every connected
/// account, exactly the number the folder pane's "All Inboxes" row shows.
/// Each is gated on its own Config toggle: the dock badge hides itself
/// when its toggle is off (it sends zero, since the dock keeps the last
/// value it was told), while a disabled tray icon was already unregistered
/// by `tray::stop`, so there's nothing to clear. Called on folder rebuilds
/// (the only thing that changes the underlying counts) and on either
/// toggle flipping; the indicator modules dedupe unchanged values and fail
/// silently without a bus or a listener that speaks the protocol.
///
/// Also withdraws the "new mail" notification (see `mail_notifications`) for
/// any notification-worthy folder whose unread count is back to 0 - some
/// desktop shells paint their own taskbar/dock badge from outstanding
/// notification counts, a mechanism separate from `LauncherEntry` above,
/// and `mail_notifications` never withdraws on its own since it has no
/// "message read" event to hook.
pub(crate) fn refresh_unread_indicators(state: &Rc<RefCell<UiState>>) {
    let (dock_enabled, tray_enabled, total, app, cleared_mailboxes) = {
        let st = state.borrow();
        let cleared_mailboxes: Vec<MailboxId> = st
            .accounts
            .values()
            .flat_map(|handle| handle.folders.iter())
            .filter(|m| crate::mail_notifications::should_notify_role(m.role) && m.unread == 0)
            .map(|m| m.id.clone())
            .collect();
        (
            st.settings.get_bool(crate::settings::DOCK_BADGE_ENABLED),
            st.settings.get_bool(crate::settings::TRAY_ICON_ENABLED),
            st.accounts
                .values()
                .filter_map(|handle| handle.folders.iter().find(|m| matches!(m.role, MailboxRole::Inbox)).map(|m| m.unread))
                .sum::<u32>(),
            st.app.clone(),
            cleared_mailboxes,
        )
    };
    crate::launcher_entry::set_unread_count(if dock_enabled { total } else { 0 });
    if tray_enabled {
        crate::tray::set_unread_count(total);
    }
    if let Some(app) = &app {
        for mailbox in &cleared_mailboxes {
            crate::mail_notifications::withdraw_new_mail_notification(app, mailbox);
        }
    }
}

/// Rebuilds the folder sidebar's `Gtk.TreeListModel` from every connected
/// account's latest folder snapshot. Accounts are sorted by email for a
/// stable order across rebuilds (`HashMap` iteration order isn't stable,
/// and accounts visibly reshuffling on every resync would be jarring).
/// On startup - before any folder has been selected or message adopted -
/// the pane opens on the user's remembered view (see `last_view`), or the
/// "All Inboxes" unified row by default (see
/// `restore_or_default_initial_view`).
pub(crate) fn rebuild_folder_tree(state: &Rc<RefCell<UiState>>, folder_selection: &gtk::SingleSelection, folder_scroller: &gtk::ScrolledWindow) {
    // Borrow only long enough to snapshot the account data. `set_selected`
    // inside `select_first_inbox` synchronously fires the `selected-item`
    // handler, which itself borrows `state` mutably - so no borrow may be
    // live across that call.
    let (auto_select_inbox, mut accounts, mut favorites): FolderTreeSnapshot = {
        let st = state.borrow();
        let accounts = st
            .accounts
            .iter()
            .map(|(id, handle)| {
                let label = if handle.display_name.is_empty() {
                    handle.email.clone()
                } else {
                    handle.display_name.clone()
                };
                (id.clone(), label, handle.folders.clone())
            })
            .collect();
        // Starred mailboxes are held as bare ids; resolve each against its
        // account's current folder list, dropping any whose folder has since
        // disappeared (or whose account hasn't connected yet).
        let favorites = st
            .favorites
            .iter()
            .filter_map(|mailbox_id| {
                let account_id = mailbox_account_id(mailbox_id)?;
                let handle = st.accounts.get(&account_id)?;
                handle.folders.iter().find(|m| &m.id == mailbox_id).cloned()
            })
            .collect();
        // Don't yank a user already in the unified view back to the first
        // account's Inbox just because some account resynced its folders.
        let auto_select_inbox = st.current_mailbox.is_none() && !matches!(st.mail_view, MailView::UnifiedInbox);
        (auto_select_inbox, accounts, favorites)
    };
    accounts.sort_by_key(|a| a.1.to_lowercase());
    // Alphabetical until the user first moves a favourite up or down, then
    // the stored order (see `move_favorite`).
    if !state.borrow().settings.get_bool(crate::settings::MAIL_FAVORITES_CUSTOM_ORDER) {
        favorites.sort_by_key(|m| m.name.to_lowercase());
    }
    state.borrow_mut().favorite_display_order = favorites.iter().map(|m| m.id.clone()).collect();

    // While the unified view is active, any account whose Inbox just appeared
    // (or reconnected) gets asked to sync so it populates the merged list.
    // Mailboxes with a sync already outstanding are skipped (`syncing`).
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        let missing: Vec<(AccountId, MailboxId)> = {
            let st = state.borrow();
            st.accounts
                .iter()
                .filter_map(|(id, handle)| {
                    handle
                        .folders
                        .iter()
                        .find(|m| matches!(m.role, MailboxRole::Inbox))
                        .filter(|m| !st.unified_snapshots.contains_key(&m.id) && !st.syncing.contains(&m.id))
                        .map(|m| (id.clone(), m.id.clone()))
                })
                .collect()
        };
        for (account_id, inbox_id) in missing {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
    }

    // Nothing the sidebar draws has changed - most `FoldersUpdated` events
    // are now folder-count refreshes for folders whose counts didn't move, so
    // this is the common case. Swapping in an identical model is not free:
    // it rebuilds every row, collapses whatever subfolders the user had
    // expanded, and drops the selection (see the restore below), so skipping
    // it outright is what keeps the pane still while counts fill in.
    let search_folders = state.borrow().app_config.borrow().search_folders.clone();
    let mut signature = folder_tree_signature(&accounts, &favorites, &state.borrow().folder_prefs);
    // Saved searches (item 2.4 / M-19) are part of the rendered tree, so they
    // belong in the signature too - a group appearing/disappearing or a name
    // changing must re-render the pane. Carried under a reserved pseudo-account
    // id, like the Favorites section.
    signature.push((
        AccountId(SEARCH_FOLDERS_GROUP_KEY.into()),
        String::new(),
        search_folders
            .iter()
            .map(|folder| (MailboxId(folder.id.clone()), folder.name.clone(), MailboxRole::Custom, '/', 0, 0, Default::default()))
            .collect(),
    ));
    {
        let st = state.borrow();
        if st.folder_tree.as_ref() == Some(&signature) && !auto_select_inbox && !st.restore_pending {
            return;
        }
    }
    // The signature guards unread counts too, so a rebuild that got this
    // far means the badge's number changed - republish it.
    refresh_unread_indicators(state);

    // The selection, the expanded subfolders, the account groups' collapse
    // state, and the scroll position don't survive `set_model`, so note them
    // all first and put them back afterwards. The scroll value matters too:
    // `GtkSingleSelection` autoselects row 0 ("All Inboxes") on `set_model`,
    // and without a restore the pane can end up jumped to the top, leaving a
    // scrolled-down account's subtree looking collapsed.
    // The capture folds the outgoing model's state into the session's durable
    // expansion memory rather than replacing it (see
    // `merge_folder_expansion`), and the restore then works from that memory.
    // On the *first* build there is no model yet (the `SingleSelection` was
    // created with `None`), so the merge is a no-op and the memory is exactly
    // what the last session left in the UI-state database.
    merge_folder_expansion(state, folder_selection);
    let (expanded, collapsed_groups) = {
        let st = state.borrow();
        (st.expanded_folders.clone(), st.collapsed_groups.clone())
    };
    let scroll_value = folder_scroller.vadjustment().value();
    let restore_to = {
        let st = state.borrow();
        match st.mail_view {
            // Row 0 is always the "All Inboxes" row.
            MailView::UnifiedInbox => Some(SelectionTarget::Unified),
            // During a search the folder pane still highlights the mailbox the
            // search started from (or nothing, if it started from the unified
            // view).
            MailView::Single | MailView::Search => st.current_mailbox.clone().map(SelectionTarget::Mailbox),
        }
    };

    let model = build_multi_account_tree_model(accounts, favorites, search_folders);
    // Everything from here to the flag being cleared is the rebuild putting
    // the pane back the way the user left it, not the user navigating - so
    // the `selected-item` handler has to stay out of it. `set_model` alone
    // fires it, autoselecting row 0 and thereby entering the unified view.
    state.borrow_mut().suppress_folder_selection = true;
    folder_selection.set_model(Some(&model));
    // Account groups must be expanded *before* `expand_mailboxes`: rows start
    // collapsed, so a fresh model's flattened list holds only depth-0 rows
    // until the groups are opened - `expand_mailboxes` could never reach a
    // `TreeItem::Folder` (all of them sit under a group) and every rebuild
    // would silently collapse the user's expanded subfolders.
    apply_account_group_expansion(&model, &collapsed_groups);
    expand_mailboxes(&model, &expanded);
    folder_scroller.vadjustment().set_value(scroll_value);
    match restore_to {
        Some(SelectionTarget::Unified) => folder_selection.set_selected(0),
        Some(SelectionTarget::Mailbox(mailbox)) => {
            if let Some(index) = find_mailbox_index(&model, &mailbox) {
                folder_selection.set_selected(index);
            }
        }
        None => {}
    }
    state.borrow_mut().suppress_folder_selection = false;

    state.borrow_mut().folder_tree = Some(signature);
    // Write the merged memory back on every rebuild, so the pane's collapse
    // state survives a restart even if the app never reaches its exit flush
    // (a crash, a kill, a session logout).
    persist_folder_expansion(state);
    if auto_select_inbox {
        restore_or_default_initial_view(state, &model, folder_selection);
    }
}

/// Fills the folder rows' shared [`crate::folder_menu::FolderMenuHandler`]
/// slot: what each "⋯" menu item (and the Trash/Junk Empty button) does. The
/// server-side actions confirm or ask for a name first where needed, then go
/// to the folder's account session as `AccountCommand`s - the tree catches up
/// from the `FoldersUpdated` that answers them. Favorites membership and
/// order, and the folder's colour and count mode, are local and apply at once.
pub(crate) fn install_folder_menu_handler(
    slot: &crate::folder_menu::FolderMenuHandler,
    state: &Rc<RefCell<UiState>>,
    folder_selection: &gtk::SingleSelection,
    folder_scroller: &gtk::ScrolledWindow,
    list_header: &ListHeader,
    worker: &Rc<Worker>,
) {
    use crate::folder_menu::{present_delete_dialog, present_empty_dialog, present_move_dialog, present_name_dialog, FolderMenuAction};
    let state = state.clone();
    let folder_selection = folder_selection.clone();
    let folder_scroller = folder_scroller.clone();
    let list_header = list_header.clone();
    let worker = worker.clone();
    let handler = move |action: FolderMenuAction, mailbox: Mailbox, anchor: gtk::Widget| {
        let Some(account_id) = mailbox_account_id(&mailbox.id) else { return };
        let send: Rc<dyn Fn(AccountCommand)> = {
            let state = state.clone();
            let account_id = account_id.clone();
            Rc::new(move |command| {
                if let Some(handle) = state.borrow().accounts.get(&account_id) {
                    let _ = handle.cmd_tx.send_blocking(command);
                }
            })
        };
        let folders = state.borrow().accounts.get(&account_id).map(|handle| handle.folders.clone()).unwrap_or_default();
        // The display names of the folders directly under `parent` (`None`:
        // the top level), for the name dialogs' duplicate check. Parents come
        // from the folder *path* - the account id itself may contain the
        // delimiter (GOA ids are object paths).
        let parent_of = |id: &MailboxId| lookout_mail::mailbox_name::parent_mailbox_id(&account_id, id, mailbox.delimiter);
        let children_of = |parent: Option<&MailboxId>| -> Vec<String> {
            folders
                .iter()
                .filter(|m| lookout_mail::mailbox_name::parent_mailbox_id(&account_id, &m.id, m.delimiter).as_ref() == parent)
                .map(|m| display_name(&m.name))
                .collect()
        };
        match action {
            FolderMenuAction::CreateSubfolder => {
                let siblings = children_of(Some(&mailbox.id));
                let parent = mailbox.id.clone();
                present_name_dialog(&anchor, "Create new subfolder", "Create", "", mailbox.delimiter, siblings, move |name| {
                    send(AccountCommand::CreateMailbox {
                        parent: Some(parent.clone()),
                        name,
                    });
                });
            }
            FolderMenuAction::Rename => {
                let siblings = children_of(parent_of(&mailbox.id).as_ref());
                let target = mailbox.id.clone();
                present_name_dialog(
                    &anchor,
                    "Rename folder",
                    "Rename",
                    &display_name(&mailbox.name),
                    mailbox.delimiter,
                    siblings,
                    move |new_name| {
                        send(AccountCommand::RenameMailbox {
                            mailbox: target.clone(),
                            new_name,
                        });
                    },
                );
            }
            FolderMenuAction::Move => {
                let target = mailbox.id.clone();
                present_move_dialog(&anchor, &mailbox, folders.clone(), move |new_parent| {
                    send(AccountCommand::MoveMailbox {
                        mailbox: target.clone(),
                        new_parent,
                    });
                });
            }
            FolderMenuAction::Delete => {
                // Mirrors the session's choice (`plan_folder_change`): moved
                // under Trash, unless it's already there or there's no Trash.
                let permanent = folders
                    .iter()
                    .find(|m| m.role == MailboxRole::Trash)
                    .is_none_or(|trash| lookout_mail::mailbox_name::is_same_or_descendant(&mailbox.id, &trash.id, mailbox.delimiter));
                let target = mailbox.id.clone();
                present_delete_dialog(&anchor, &mailbox, permanent, move || send(AccountCommand::DeleteMailbox { mailbox: target.clone() }));
            }
            FolderMenuAction::Empty => {
                let target = mailbox.id.clone();
                present_empty_dialog(&anchor, &mailbox, move || send(AccountCommand::EmptyMailbox { mailbox: target.clone() }));
            }
            FolderMenuAction::MarkAllRead => send(AccountCommand::MarkMailboxRead { mailbox: mailbox.id.clone() }),
            FolderMenuAction::RunRules => crate::rules::run_rules_now(&state, &worker, &account_id, &mailbox.id),
            FolderMenuAction::AddFavorite | FolderMenuAction::RemoveFavorite => {
                set_folder_favorite(&state, &mailbox.id, action == FolderMenuAction::AddFavorite);
                // The header's star follows when this is the open folder.
                refresh_list_header(&state, &list_header);
                rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
            }
            FolderMenuAction::MoveFavoriteUp | FolderMenuAction::MoveFavoriteDown => {
                let delta = if action == FolderMenuAction::MoveFavoriteUp { -1 } else { 1 };
                if move_favorite(&state, &mailbox.id, delta) {
                    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
                }
            }
            FolderMenuAction::SetColor(_) | FolderMenuAction::SetCountMode(_) => {
                {
                    let mut st = state.borrow_mut();
                    let prefs = st.folder_prefs.entry(mailbox.id.clone()).or_default();
                    match action {
                        FolderMenuAction::SetColor(color) => prefs.color = color,
                        FolderMenuAction::SetCountMode(mode) => prefs.count_mode = mode,
                        _ => unreachable!("matched above"),
                    }
                    if prefs.is_default() {
                        st.folder_prefs.remove(&mailbox.id);
                    }
                }
                persist_folder_prefs(&state);
                rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
            }
        }
    };
    *slot.borrow_mut() = Some(Rc::new(handler));
}

pub(crate) fn persist_folder_prefs(state: &Rc<RefCell<UiState>>) {
    let st = state.borrow();
    if let Some(db) = &st.ui_db {
        if let Err(e) = db.set_folder_prefs(&st.folder_prefs) {
            tracing::warn!("folder colours and count modes won't persist: {e}");
        }
    }
}

/// Re-keys everything the UI holds by mailbox id after the folder `from` was
/// renamed or moved to `to` (its subfolders move with it): the Favorites
/// list, the folder prefs, the expanded-subfolder memory, the recent move
/// targets, and the open folder (with its remembered last view). The session has already re-pointed its
/// own open folder and resyncs it under the new id.
pub(crate) fn rekey_renamed_folder(state: &Rc<RefCell<UiState>>, from: &MailboxId, to: &MailboxId, delimiter: char) {
    use lookout_mail::mailbox_name::rekey_mailbox_id;
    {
        let mut st = state.borrow_mut();
        st.favorites = crate::folder_menu::rekey_ids(&st.favorites, from, to, delimiter);
        st.folder_prefs = std::mem::take(&mut st.folder_prefs)
            .into_iter()
            .map(|(id, prefs)| (rekey_mailbox_id(&id, from, to, delimiter).unwrap_or(id), prefs))
            .collect();
        st.expanded_folders = std::mem::take(&mut st.expanded_folders)
            .into_iter()
            .map(|id| rekey_mailbox_id(&id, from, to, delimiter).unwrap_or(id))
            .collect();
        for recent in st.recent_folders.values_mut() {
            for id in recent.iter_mut() {
                if let Some(rekeyed) = rekey_mailbox_id(id, from, to, delimiter) {
                    *id = rekeyed;
                }
            }
        }
        if let Some(current) = st.current_mailbox.as_ref().and_then(|current| rekey_mailbox_id(current, from, to, delimiter)) {
            st.current_mailbox = Some(current.clone());
            if matches!(st.mail_view, MailView::Single) {
                last_view::save(
                    &st.settings,
                    &LastSelection {
                        unified: false,
                        mailbox: Some(current.0),
                    },
                );
            }
        }
    }
    persist_favorites(state);
    persist_folder_prefs(state);
    persist_folder_expansion(state);
    persist_recent_folders(state, from);
}

/// Writes the recent move targets of `folder`'s account back to the UI-state
/// database (after a folder rename or delete changed them).
pub(crate) fn persist_recent_folders(state: &Rc<RefCell<UiState>>, folder: &MailboxId) {
    let Some(account_id) = mailbox_account_id(folder) else { return };
    let st = state.borrow();
    if let Some(db) = &st.ui_db {
        let recent = st.recent_folders.get(&account_id).cloned().unwrap_or_default();
        if let Err(e) = db.set_recent_folders(&account_id, &recent) {
            tracing::warn!("recent folders won't persist: {e}");
        }
    }
}

/// Forgets everything the UI holds for the folder `root` and its subfolders
/// after they were permanently deleted. If the open folder was among them,
/// the view moves to the account's Inbox (which the session resyncs).
pub(crate) fn forget_deleted_folder(state: &Rc<RefCell<UiState>>, root: &MailboxId, delimiter: char) {
    use lookout_mail::mailbox_name::is_same_or_descendant;
    {
        let mut st = state.borrow_mut();
        st.favorites.retain(|id| !is_same_or_descendant(id, root, delimiter));
        st.folder_prefs.retain(|id, _| !is_same_or_descendant(id, root, delimiter));
        st.expanded_folders.retain(|id| !is_same_or_descendant(id, root, delimiter));
        for recent in st.recent_folders.values_mut() {
            recent.retain(|id| !is_same_or_descendant(id, root, delimiter));
        }
        let open_was_deleted = st.current_mailbox.as_ref().is_some_and(|current| is_same_or_descendant(current, root, delimiter));
        if open_was_deleted {
            let inbox = mailbox_account_id(root)
                .and_then(|account_id| st.accounts.get(&account_id))
                .and_then(|handle| handle.folders.iter().find(|m| m.role == MailboxRole::Inbox))
                .map(|inbox| inbox.id.clone());
            st.current_mailbox = inbox.clone();
            if matches!(st.mail_view, MailView::Single) {
                last_view::save(
                    &st.settings,
                    &LastSelection {
                        unified: false,
                        mailbox: inbox.map(|id| id.0),
                    },
                );
            }
        }
    }
    persist_favorites(state);
    persist_folder_prefs(state);
    persist_folder_expansion(state);
    persist_recent_folders(state, root);
}

/// Adds `mailbox` to, or removes it from, the folder tree's Favorites
/// section, writing the whole list through to `mail-favorites` so it survives
/// restarts. A new favourite goes at the end. Shared by the message-list
/// header's star and the folder rows' "⋯" menu; the caller rebuilds the tree.
/// The open folder, as the Folder ribbon tab acts on it, and which of the
/// folder menu's commands it allows (the same rules as its right-click menu).
/// `None` in the unified Inbox, or before a folder is open.
pub(crate) fn current_folder(state: &Rc<RefCell<UiState>>) -> Option<(Mailbox, crate::folder_menu::FolderMenuState)> {
    let st = state.borrow();
    if matches!(st.mail_view, MailView::UnifiedInbox) {
        return None;
    }
    let id = st.current_mailbox.as_ref()?;
    let handle = st.accounts.get(&mailbox_account_id(id)?)?;
    let mailbox = handle.folders.iter().find(|m| &m.id == id)?.clone();
    let order = &st.favorite_display_order;
    let favorite_pos = order.iter().position(|m| m == id).map(|index| (index, order.len()));
    let menu_state = crate::folder_menu::folder_menu_state(&mailbox, favorite_pos, !handle.uses_graph_mail);
    Some((mailbox, menu_state))
}

pub(crate) fn set_folder_favorite(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, favorite: bool) {
    {
        let mut st = state.borrow_mut();
        if favorite {
            if !st.favorites.contains(mailbox) {
                st.favorites.push(mailbox.clone());
            }
        } else {
            st.favorites.retain(|m| m != mailbox);
        }
    }
    persist_favorites(state);
}

pub(crate) fn persist_favorites(state: &Rc<RefCell<UiState>>) {
    let st = state.borrow();
    st.settings.set_strv(crate::settings::MAIL_FAVORITES, st.favorites.iter().map(|m| m.0.clone()).collect());
}

/// Moves a favourite one place up (`-1`) or down (`+1`) in the Favorites
/// section as the user sees it. The first reorder switches the section from
/// alphabetical to the stored order (`mail-favorites-custom-order`), so the
/// move starts from the order on screen; favourites whose folder isn't
/// listed right now (an account still connecting) keep their place at the
/// end. Returns whether anything moved; the caller rebuilds the tree.
pub(crate) fn move_favorite(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, delta: isize) -> bool {
    {
        let mut st = state.borrow_mut();
        let mut order = st.favorite_display_order.clone();
        if !crate::folder_menu::move_in_order(&mut order, mailbox, delta) {
            return false;
        }
        let unlisted: Vec<MailboxId> = st.favorites.iter().filter(|m| !order.contains(m)).cloned().collect();
        order.extend(unlisted);
        st.favorites = order;
        st.settings.set_bool(crate::settings::MAIL_FAVORITES_CUSTOM_ORDER, true);
    }
    persist_favorites(state);
    true
}

/// Where `rebuild_folder_tree` puts the sidebar highlight back after swapping
/// the model: the pinned "All Inboxes" row, or the open mailbox's own row.
pub(crate) enum SelectionTarget {
    Unified,
    Mailbox(MailboxId),
}

/// One walk of the live folder pane, recording both halves of what the pane
/// can currently say about its own collapse state: which rows are
/// expanded/collapsed, *and* which rows were there to be asked at all.
///
/// The second half is what makes the memory safe to merge. A `TreeListModel`
/// only materializes rows whose ancestors are expanded, and an account only
/// grows folder rows once its folder list has arrived - so at any moment the
/// pane is silent about most of what the user has ever expanded. Treating
/// that silence as "not expanded" is what wiped the memory: the empty tree
/// the app builds before any account has connected would answer "nothing is
/// expanded, nothing is collapsed" and overwrite the previous session's
/// state within milliseconds of launch. A row the model can't speak for is
/// left exactly as the memory has it.
pub(crate) struct FolderExpansionCapture {
    /// Materialized `TreeItem::Folder` rows that are expanded.
    pub(crate) expanded: HashSet<MailboxId>,
    /// Materialized `TreeItem::Folder` rows that *could* be expanded - the
    /// only mailboxes this capture is entitled to overwrite. A folder with no
    /// subfolders has no expansion state to record, so it is excluded and its
    /// remembered state survives until its subfolders come back.
    pub(crate) expandable: HashSet<MailboxId>,
    /// Depth-0 account groups (and the Favorites section, under
    /// `FAVORITES_GROUP_KEY`) that are collapsed.
    pub(crate) collapsed_groups: HashSet<AccountId>,
    /// Depth-0 groups that are genuinely collapsible - the groups this
    /// capture may overwrite. An account whose folders haven't landed yet
    /// renders as a non-expandable row that the user cannot have collapsed,
    /// so it is excluded and keeps whatever the memory holds for it.
    pub(crate) collapsible_groups: HashSet<AccountId>,
}

pub(crate) fn capture_folder_expansion(folder_selection: &gtk::SingleSelection) -> Option<FolderExpansionCapture> {
    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>()?;
    let mut capture = FolderExpansionCapture {
        expanded: HashSet::new(),
        expandable: HashSet::new(),
        collapsed_groups: HashSet::new(),
        collapsible_groups: HashSet::new(),
    };
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
        let tree_item = boxed.borrow::<TreeItem>();
        match &*tree_item {
            TreeItem::Folder(node) => {
                if row.is_expanded() {
                    capture.expanded.insert(node.mailbox.id.clone());
                }
                if row.is_expandable() {
                    capture.expandable.insert(node.mailbox.id.clone());
                }
            }
            item @ (TreeItem::Account(_) | TreeItem::Favorites) => {
                if row.depth() != 0 || !row.is_expandable() {
                    continue;
                }
                let key = match item {
                    TreeItem::Account(acc) => acc.account_id.clone(),
                    _ => AccountId(FAVORITES_GROUP_KEY.into()),
                };
                if !row.is_expanded() {
                    capture.collapsed_groups.insert(key.clone());
                }
                capture.collapsible_groups.insert(key);
            }
            _ => {}
        }
    }
    Some(capture)
}

/// Folds the live pane's collapse state into the session's durable expansion
/// memory (`UiState::expanded_folders` / `collapsed_groups`), which is what
/// both the rebuild restore and the database write work from.
///
/// Merge, never replace: only the rows the capture actually saw are
/// overwritten (see [`FolderExpansionCapture`]). A no-op before the first
/// model exists, so the startup memory loaded from the database reaches the
/// first build intact.
pub(crate) fn merge_folder_expansion(state: &Rc<RefCell<UiState>>, folder_selection: &gtk::SingleSelection) {
    let Some(capture) = capture_folder_expansion(folder_selection) else {
        return;
    };
    let mut st = state.borrow_mut();
    st.expanded_folders.retain(|id| !capture.expandable.contains(id));
    st.expanded_folders.extend(capture.expanded);
    st.collapsed_groups.retain(|id| !capture.collapsible_groups.contains(id));
    st.collapsed_groups.extend(capture.collapsed_groups);
}

/// Writes the session's expansion memory to the UI-state database.
/// Best-effort like the other UI-state tables: a failed open or write means
/// the pane's collapse state is session-only, never an error.
pub(crate) fn persist_folder_expansion(state: &Rc<RefCell<UiState>>) {
    let (db, expanded, collapsed) = {
        let st = state.borrow();
        (st.ui_db.clone(), st.expanded_folders.clone(), st.collapsed_groups.clone())
    };
    if let Some(db) = db {
        if let Err(e) = db.set_folder_expansion(&expanded, &collapsed) {
            tracing::warn!("folder expansion state won't persist: {e}");
        }
    }
}

/// Re-expands the rows named by `expanded` in a freshly built model. Walks by
/// index rather than over a snapshot because expanding a row inserts its
/// children into the model, and those children may themselves need expanding.
/// Must run *after* `apply_account_group_expansion`: the new model's rows all
/// start collapsed, so the flattened list only reaches folder rows once their
/// account group (and any ancestor folder) is expanded.
pub(crate) fn expand_mailboxes(model: &gtk::TreeListModel, expanded: &HashSet<MailboxId>) {
    if expanded.is_empty() {
        return;
    }
    let mut i = 0;
    while i < model.n_items() {
        if let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() {
            if let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() {
                let is_wanted = matches!(&*boxed.borrow::<TreeItem>(), TreeItem::Folder(node) if expanded.contains(&node.mailbox.id));
                if is_wanted {
                    row.set_expanded(true);
                }
            }
        }
        i += 1;
    }
}

/// The reserved pseudo-account key under which the Favorites section's group
/// state is carried by `capture_folder_expansion` - the same sentinel
/// `folder_tree_signature` uses to fold the favorites section into its
/// comparison.
pub(crate) const FAVORITES_GROUP_KEY: &str = "\u{0}favorites";

/// The reserved pseudo-account key under which the Search Folders group's
/// membership is carried in the folder-tree signature (item 2.4 / M-19).
pub(crate) const SEARCH_FOLDERS_GROUP_KEY: &str = "\u{0}search-folders";

/// The account groups (and the Favorites section) the user has collapsed in
/// the current tree. Test-only sugar over [`capture_folder_expansion`]; the
/// pane itself never asks the live model directly, it works from the merged
/// session memory (see [`merge_folder_expansion`]).
#[cfg(test)]
pub(crate) fn collapsed_account_groups(folder_selection: &gtk::SingleSelection) -> HashSet<AccountId> {
    capture_folder_expansion(folder_selection).map(|c| c.collapsed_groups).unwrap_or_default()
}

/// Applies the account groups' (and the Favorites section's) collapse state
/// to a freshly built model, replacing `build_multi_account_tree_model`'s old
/// unconditional expand-all. Rows the user collapsed stay collapsed;
/// everything else - including accounts that just connected - defaults to
/// expanded, the long-standing look of the pane. Only depth-0 rows are
/// touched: subfolders are the caller's `expand_mailboxes` restore's job,
/// which must run *after* this (its rows only materialize once their group
/// is open).
/// Walks by index with `n_items` re-evaluated (like `expand_mailboxes`)
/// because expanding a row inserts its children into the flat model, shifting
/// every later row's position; a fixed range would silently skip them.
pub(crate) fn apply_account_group_expansion(model: &gtk::TreeListModel, collapsed: &HashSet<AccountId>) {
    let mut i = 0;
    while i < model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else {
            i += 1;
            continue;
        };
        if row.depth() != 0 {
            i += 1;
            continue;
        }
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
            i += 1;
            continue;
        };
        let tree_item = boxed.borrow::<TreeItem>();
        let key = match &*tree_item {
            TreeItem::Account(acc) => Some(acc.account_id.clone()),
            TreeItem::Favorites => Some(AccountId(FAVORITES_GROUP_KEY.into())),
            _ => None,
        };
        if let Some(key) = key {
            if !collapsed.contains(&key) {
                row.set_expanded(true);
            }
        }
        i += 1;
    }
}

/// Chooses the folder pane's starting selection on first load. A remembered
/// view (see `last_view`) is restored when its mailbox is present in the
/// rebuilt tree; a remembered "All Inboxes" restores the unified row. With
/// no memory the pane defaults to "All Inboxes" (row 0). If the remembered
/// mailbox's account hasn't connected yet, the restore is deferred - this
/// does nothing and `rebuild_folder_tree` runs it again on the next
/// `FoldersUpdated`, so a slow account can't lose the user's folder. Only
/// once that account has delivered folders and still doesn't list the
/// mailbox is the restore abandoned for the unified default.
pub(crate) fn restore_or_default_initial_view(state: &Rc<RefCell<UiState>>, model: &gtk::TreeListModel, folder_selection: &gtk::SingleSelection) {
    // Decide without holding a borrow: `set_selected` synchronously fires the
    // `selected-item` handler, which itself borrows `state` mutably.
    let (action, done) = {
        let mut st = state.borrow_mut();
        let action = match st.last_selection.clone() {
            None => Some(0),
            Some(sel) if sel.unified => Some(0),
            Some(sel) => match sel.mailbox {
                Some(mailbox) => {
                    let mailbox_id = MailboxId(mailbox);
                    if let Some(index) = find_mailbox_index(model, &mailbox_id) {
                        Some(index)
                    } else {
                        // Not visible yet. If the mailbox's account has
                        // connected and still doesn't list it, the folder is
                        // gone - fall back to the unified default rather than
                        // waiting forever. Otherwise keep waiting for the
                        // account to connect.
                        let account_gone = mailbox_account_id(&mailbox_id)
                            .and_then(|id| st.accounts.get(&id))
                            .map(|h| !h.folders.is_empty())
                            .unwrap_or(false);
                        account_gone.then_some(0)
                    }
                }
                None => Some(0),
            },
        };
        let done = action.is_some();
        if done {
            st.restore_pending = false;
        }
        (action, done)
    };
    if done {
        if let Some(index) = action {
            folder_selection.set_selected(index);
        }
    }
}

/// Finds the flattened index of a specific mailbox row in the folder tree's
/// `TreeListModel`, by exact `MailboxId`. On the flat `TreeListModel`,
/// iterating top-level indices reaches folder rows inside expanded account
/// groups. Used to restore the remembered view on startup and to put the
/// highlight back after a favorite toggle rebuilds the tree.
///
/// Deliberately matches only `TreeItem::Folder`: a starred mailbox also has a
/// `TreeItem::Favorite` row in the pinned Favorites section, which sorts
/// *above* its real row, so matching both would resolve every favorite to its
/// duplicate.
pub(crate) fn find_mailbox_index(model: &gtk::TreeListModel, mailbox_id: &MailboxId) -> Option<u32> {
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else {
            continue;
        };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
            continue;
        };
        let tree_item = boxed.borrow::<TreeItem>();
        if let TreeItem::Folder(node) = &*tree_item {
            if &node.mailbox.id == mailbox_id {
                return Some(i);
            }
        }
    }
    None
}

/// Maps a mailbox's special-use role to a folder-row icon name, mirroring the
/// role->icon mapping in the reference webmail app's `getIconForMailbox()`
/// (webmail/components/layout/sidebar.tsx). These specific names come from
/// this machine's Yaru icon theme, not the base Adwaita theme - fine for now
/// since there's no working Flatpak build yet, just a manifest spike.
pub(crate) fn folder_icon_name(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "mail-inbox-symbolic",
        MailboxRole::Sent => "mail-sent-symbolic",
        MailboxRole::Drafts => "mail-drafts-symbolic",
        MailboxRole::Trash => "user-trash-symbolic",
        MailboxRole::Junk => "mail-spam-symbolic",
        MailboxRole::Archive => "mail-archive-symbolic",
        MailboxRole::Custom => "folder-symbolic",
    }
}
