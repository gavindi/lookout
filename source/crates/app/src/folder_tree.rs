/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, Mailbox, MailboxRole};

/// A folder-tree node, built once per `FoldersUpdated` event and wrapped in
/// `glib::BoxedAnyObject` for use in a `Gtk.TreeListModel`. Kept separate
/// from `lookout_core::Mailbox` because parent/child linking is inherently a
/// UI-layer concern (see `lookout_mail::session::list_mailboxes`'s doc
/// comment on `Mailbox::parent`) - `lookout-core` only carries the flat data.
pub struct FolderNode {
    pub mailbox: Mailbox,
    pub children: Vec<Rc<FolderNode>>,
}

/// A root-level row identifying one connected GOA account, above that
/// account's own folder tree.
pub struct AccountNode {
    pub account_id: AccountId,
    pub label: String,
    /// The account's Inbox unread count, shown at the row's trailing edge so
    /// a collapsed account still reports whether it has new mail. Its Inbox
    /// rather than a sum over every folder: a Junk folder with four thousand
    /// unread would otherwise be all this number ever says.
    pub unread: u32,
}

/// What a `Gtk.TreeListRow`'s `glib::BoxedAnyObject` actually holds - the
/// synthetic "All Inboxes" unified row pinned above every account group, an
/// account-grouping row itself, or one of that account's mailboxes.
pub enum TreeItem {
    /// The virtual "All Inboxes" folder: merges every connected account's
    /// Inbox into one cross-account message list. A leaf row (no children).
    /// Carries the summed unread count of the inboxes it merges, since it is
    /// a real destination the user reads mail in, not just a grouping row.
    Unified(u32),
    /// The "Favorites" grouping row, pinned between "All Inboxes" and the
    /// account groups. Present only while at least one folder is starred.
    Favorites,
    Account(AccountNode),
    Folder(Rc<FolderNode>),
    /// A starred mailbox, shown inside the Favorites section. Selecting one
    /// behaves exactly like selecting its `Folder` row - but it is a *second*
    /// row for a mailbox that also appears under its account, so anything
    /// resolving a `MailboxId` back to a tree position must skip these or it
    /// will land on the favorite copy instead of the folder's real row (see
    /// `find_mailbox_index`).
    Favorite(Rc<FolderNode>),
}

/// Reconstructs a folder hierarchy from the flat mailbox list IMAP's `LIST`
/// returns, by splitting each mailbox's full path (recovered from its
/// `MailboxId`) on its hierarchy delimiter. A folder whose parent path isn't
/// itself present in the fetched set (some servers omit intermediate
/// \Noselect containers) is simply promoted to a root - documented
/// simplification rather than a synthetic placeholder node.
pub fn build_folder_roots(mailboxes: Vec<Mailbox>, account_id: &AccountId) -> Vec<Rc<FolderNode>> {
    let prefix = format!("{}:", account_id.0);

    let mut by_path: HashMap<String, Mailbox> = HashMap::new();
    for m in mailboxes {
        if let Some(path) = m.id.0.strip_prefix(&prefix) {
            by_path.insert(path.to_string(), m);
        }
    }

    let mut children_of: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for (path, mailbox) in &by_path {
        let parent = parent_path(path, mailbox.delimiter);
        let parent = parent.filter(|p| by_path.contains_key(p));
        children_of.entry(parent).or_default().push(path.clone());
    }

    fn build(path: &str, by_path: &HashMap<String, Mailbox>, children_of: &HashMap<Option<String>, Vec<String>>) -> Rc<FolderNode> {
        let mailbox = by_path[path].clone();
        let mut child_paths = children_of.get(&Some(path.to_string())).cloned().unwrap_or_default();
        sort_paths(&mut child_paths, by_path);
        let children = child_paths.iter().map(|p| build(p, by_path, children_of)).collect();
        Rc::new(FolderNode { mailbox, children })
    }

    let mut root_paths = children_of.get(&None).cloned().unwrap_or_default();
    sort_paths(&mut root_paths, &by_path);
    root_paths.iter().map(|p| build(p, &by_path, &children_of)).collect()
}

/// One account's Inbox unread count - the number its account row and the
/// unified row are built from. Zero when the account has no Inbox (it hasn't
/// finished connecting, or the server names it something unrecognized).
fn inbox_unread(mailboxes: &[Mailbox]) -> u32 {
    mailboxes.iter().find(|m| matches!(m.role, MailboxRole::Inbox)).map_or(0, |m| m.unread)
}

fn parent_path(path: &str, delimiter: char) -> Option<String> {
    path.rsplit_once(delimiter).map(|(parent, _)| parent.to_string())
}

/// Inbox first (Bulwark/most mail clients pin it), then alphabetical by
/// display name - simple and predictable for Phase 1.
fn sort_paths(paths: &mut [String], by_path: &HashMap<String, Mailbox>) {
    paths.sort_by(|a, b| {
        let ma = &by_path[a];
        let mb = &by_path[b];
        let a_is_inbox = matches!(ma.role, MailboxRole::Inbox);
        let b_is_inbox = matches!(mb.role, MailboxRole::Inbox);
        b_is_inbox.cmp(&a_is_inbox).then_with(|| ma.name.to_lowercase().cmp(&mb.name.to_lowercase()))
    });
}

/// Builds the `Gtk.TreeListModel` for the folder sidebar from every
/// connected account's flat mailbox list: the synthetic "All Inboxes" unified
/// row pinned at the top, the "Favorites" section (omitted when nothing is
/// starred), then one top-level `TreeItem::Account` row per account
/// (Thunderbird-style grouping), each expanding to that account's own folder
/// tree. Each row's item is a `glib::BoxedAnyObject` wrapping a `TreeItem`.
///
/// Rows are built *collapsed*; `rebuild_folder_tree` in `window.rs` applies
/// the session's expansion state afterwards (every account group and the
/// Favorites section expanded by default on the first build) - the same
/// split `MessageListModel` uses, so a user's manual account-group collapse
/// survives the constant `FoldersUpdated` rebuilds instead of being
/// re-derived from scratch.
///
/// `favorites` are rendered as flat leaves - a favorite is one folder, not a
/// subtree - and are duplicates of rows that also live under their account.
pub fn build_multi_account_tree_model(accounts: Vec<(AccountId, String, Vec<Mailbox>)>, favorites: Vec<Mailbox>) -> gtk::TreeListModel {
    let mut folder_roots_by_account: HashMap<AccountId, Vec<Rc<FolderNode>>> = HashMap::new();
    let root_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    // The unified row merges exactly the inboxes the account rows count, so
    // its number is their sum - computed before `accounts` is consumed below.
    let unified_unread: u32 = accounts.iter().map(|(_, _, mailboxes)| inbox_unread(mailboxes)).sum();
    root_store.append(&glib::BoxedAnyObject::new(TreeItem::Unified(unified_unread)));
    let favorite_nodes: Vec<Rc<FolderNode>> = favorites.into_iter().map(|mailbox| Rc::new(FolderNode { mailbox, children: Vec::new() })).collect();
    if !favorite_nodes.is_empty() {
        root_store.append(&glib::BoxedAnyObject::new(TreeItem::Favorites));
    }
    for (account_id, label, mailboxes) in accounts {
        let unread = inbox_unread(&mailboxes);
        let roots = build_folder_roots(mailboxes, &account_id);
        root_store.append(&glib::BoxedAnyObject::new(TreeItem::Account(AccountNode {
            account_id: account_id.clone(),
            label,
            unread,
        })));
        folder_roots_by_account.insert(account_id, roots);
    }

    let model = gtk::TreeListModel::new(root_store, false, false, move |item| {
        let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
        let tree_item = boxed.borrow::<TreeItem>();
        // The Favorites section's children are `Favorite` rows, not `Folder`
        // ones, so the tree-index scanners can tell the starred duplicate
        // apart from the mailbox's real row under its account.
        if matches!(&*tree_item, TreeItem::Favorites) {
            let store = gio::ListStore::new::<glib::BoxedAnyObject>();
            for node in &favorite_nodes {
                store.append(&glib::BoxedAnyObject::new(TreeItem::Favorite(node.clone())));
            }
            return Some(store.upcast::<gio::ListModel>());
        }
        let children = match &*tree_item {
            TreeItem::Unified(_) | TreeItem::Favorites | TreeItem::Favorite(_) => return None,
            TreeItem::Account(acc) => folder_roots_by_account.get(&acc.account_id)?.clone(),
            TreeItem::Folder(node) => node.children.clone(),
        };
        if children.is_empty() {
            // An account whose folder list hasn't arrived yet (it's still
            // connecting, or its session restarted) must render as *expandable
            // but empty* rather than a leaf: a leaf row can't be expanded, so
            // a slow-connecting account would otherwise look permanently
            // collapsed - with no chevron to reopen it - until its folders
            // land. An empty child store keeps the row expandable, holds its
            // expanded state (the next rebuild fills the store with content),
            // and shows nothing beneath it in the meantime. A `Folder` row
            // stays a leaf - a folder genuinely can have no subfolders.
            if matches!(&*tree_item, TreeItem::Account(_)) {
                let store = gio::ListStore::new::<glib::BoxedAnyObject>();
                return Some(store.upcast::<gio::ListModel>());
            }
            return None;
        }
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for child in children {
            store.append(&glib::BoxedAnyObject::new(TreeItem::Folder(child)));
        }
        Some(store.upcast::<gio::ListModel>())
    });

    model
}
