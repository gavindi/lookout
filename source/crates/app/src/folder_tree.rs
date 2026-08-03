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
}

/// What a `Gtk.TreeListRow`'s `glib::BoxedAnyObject` actually holds - the
/// synthetic "All Inboxes" unified row pinned above every account group, an
/// account-grouping row itself, or one of that account's mailboxes.
pub enum TreeItem {
    /// The virtual "All Inboxes" folder: merges every connected account's
    /// Inbox into one cross-account message list. A leaf row (no children).
    Unified,
    Account(AccountNode),
    Folder(Rc<FolderNode>),
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
/// row pinned at the top, then one top-level, expanded-by-default
/// `TreeItem::Account` row per account (Thunderbird-style grouping), each
/// expanding to that account's own folder tree. Each row's item is a
/// `glib::BoxedAnyObject` wrapping a `TreeItem`.
pub fn build_multi_account_tree_model(accounts: Vec<(AccountId, String, Vec<Mailbox>)>) -> gtk::TreeListModel {
    let mut folder_roots_by_account: HashMap<AccountId, Vec<Rc<FolderNode>>> = HashMap::new();
    let root_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    root_store.append(&glib::BoxedAnyObject::new(TreeItem::Unified));
    for (account_id, label, mailboxes) in accounts {
        let roots = build_folder_roots(mailboxes, &account_id);
        root_store.append(&glib::BoxedAnyObject::new(TreeItem::Account(AccountNode {
            account_id: account_id.clone(),
            label,
        })));
        folder_roots_by_account.insert(account_id, roots);
    }

    let model = gtk::TreeListModel::new(root_store, false, false, move |item| {
        let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
        let tree_item = boxed.borrow::<TreeItem>();
        let children = match &*tree_item {
            TreeItem::Unified => return None,
            TreeItem::Account(acc) => folder_roots_by_account.get(&acc.account_id)?.clone(),
            TreeItem::Folder(node) => node.children.clone(),
        };
        if children.is_empty() {
            return None;
        }
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for child in children {
            store.append(&glib::BoxedAnyObject::new(TreeItem::Folder(child)));
        }
        Some(store.upcast::<gio::ListModel>())
    });

    // Expand every account row by default so folders are immediately
    // visible without an extra click - matches the single-account
    // behavior this replaces. Only the top level: subfolders still start
    // collapsed. The "All Inboxes" leaf at index 0 has no children to
    // expand, so it's skipped.
    for i in 1..model.n_items() {
        if let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() {
            row.set_expanded(true);
        }
    }

    model
}
