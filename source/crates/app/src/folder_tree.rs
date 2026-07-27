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

/// Builds the `Gtk.TreeListModel` for the folder sidebar from a flat mailbox
/// list. Each row's item is a `glib::BoxedAnyObject` wrapping an
/// `Rc<FolderNode>`.
pub fn build_tree_model(mailboxes: Vec<Mailbox>, account_id: &AccountId) -> gtk::TreeListModel {
    let roots = build_folder_roots(mailboxes, account_id);
    let root_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for node in roots {
        root_store.append(&glib::BoxedAnyObject::new(node));
    }

    gtk::TreeListModel::new(root_store, false, false, |item| {
        let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
        let node = boxed.borrow::<Rc<FolderNode>>();
        if node.children.is_empty() {
            return None;
        }
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for child in &node.children {
            store.append(&glib::BoxedAnyObject::new(child.clone()));
        }
        Some(store.upcast::<gio::ListModel>())
    })
}
