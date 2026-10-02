//! The folder pane's per-row "⋯" menu (Mail tab).
//!
//! A flat button at the trailing edge of every folder row, shown while the
//! row is hovered (or while its menu is open), opening an
//! Outlook-style menu: the folder's item counts, create/rename/move/delete,
//! a colour and a count-mode for the row, Empty, Mark all as read, and the
//! Favorites membership and order.
//!
//! This module owns everything that doesn't need the window's state: the
//! menu widget and its per-row action group ([`RowMenu`]), the enable rules
//! ([`folder_menu_state`]), the dialogs, and the pure helpers for reordering
//! favourites and re-keying ids after a rename. What an action *does* is
//! decided by a [`FolderMenuHandler`] that `window.rs` installs once its state
//! exists - the factory builds rows long before that.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{display_name, AccountId, Mailbox, MailboxId, MailboxRole};
use lookout_mail::mailbox_name::{is_same_or_descendant, parent_mailbox_id, rekey_mailbox_id};

use crate::calendar_colors::CALENDAR_PALETTE;
use crate::ui_state_db::FolderCountMode;

/// One thing the menu can ask for. Carried to the [`FolderMenuHandler`]
/// together with the row's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderMenuAction {
    CreateSubfolder,
    Rename,
    Move,
    Delete,
    Empty,
    MarkAllRead,
    AddFavorite,
    RemoveFavorite,
    MoveFavoriteUp,
    MoveFavoriteDown,
    SetColor(Option<String>),
    SetCountMode(FolderCountMode),
}

/// What runs a [`FolderMenuAction`] for a folder. The widget is the row's menu
/// button, for dialogs to present against. Filled in by `window.rs` after the
/// window state exists; `None` until then (and rows ignore clicks meanwhile).
pub type FolderMenuHandler = Rc<RefCell<Option<Rc<dyn Fn(FolderMenuAction, Mailbox, gtk::Widget)>>>>;

/// Which of the menu's items are available for one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderMenuState {
    pub create: bool,
    pub rename: bool,
    pub move_folder: bool,
    pub delete: bool,
    pub empty: bool,
    pub mark_read: bool,
    pub is_favorite: bool,
    pub move_up: bool,
    pub move_down: bool,
}

/// The menu's enable rules. The special folders (Inbox, Sent, Drafts, Trash,
/// Junk, Archive) can't be renamed, moved or deleted; a `\Noinferiors` folder
/// can't hold subfolders; Empty is the permanent Trash/Junk expunge; Move
/// up/down reorder the Favorites section, so they need the folder to be a
/// favourite (`favorite_pos` is its `(index, count)` there) and not already at
/// that end.
pub fn folder_menu_state(mailbox: &Mailbox, favorite_pos: Option<(usize, usize)>) -> FolderMenuState {
    let custom = mailbox.role == MailboxRole::Custom;
    FolderMenuState {
        create: !mailbox.flags.iter().any(|flag| flag.eq_ignore_ascii_case("NoInferiors")),
        rename: custom,
        move_folder: custom,
        delete: custom,
        empty: matches!(mailbox.role, MailboxRole::Trash | MailboxRole::Junk),
        mark_read: mailbox.unread > 0,
        is_favorite: favorite_pos.is_some(),
        move_up: favorite_pos.is_some_and(|(index, _)| index > 0),
        move_down: favorite_pos.is_some_and(|(index, count)| index + 1 < count),
    }
}

/// The menu's heading line, e.g. "35 items (2 unread)".
pub fn folder_info_text(mailbox: &Mailbox) -> String {
    let items = if mailbox.total == 1 { "item" } else { "items" };
    format!("{} {items} ({} unread)", mailbox.total, mailbox.unread)
}

/// Why `name` can't be used for a folder among `siblings` (the display names
/// already beside it), or `None` if it can. Mirrors the session's own checks
/// so the dialog can say so before anything is sent.
pub fn folder_name_problem(name: &str, delimiter: char, siblings: &[String], current: Option<&str>) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return Some(String::new());
    }
    if name.contains(delimiter) {
        return Some(format!("Folder names can't contain \u{201c}{delimiter}\u{201d}"));
    }
    let taken = siblings
        .iter()
        .filter(|sibling| current.is_none_or(|current| !sibling.eq_ignore_ascii_case(current)))
        .any(|sibling| sibling.eq_ignore_ascii_case(name));
    taken.then(|| format!("A folder called \u{201c}{name}\u{201d} already exists here"))
}

/// Moves `id` one place up (`-1`) or down (`+1`) in `order`. Returns whether
/// anything moved.
pub fn move_in_order(order: &mut [MailboxId], id: &MailboxId, delta: isize) -> bool {
    let Some(index) = order.iter().position(|m| m == id) else { return false };
    let Some(target) = index.checked_add_signed(delta).filter(|target| *target < order.len()) else {
        return false;
    };
    order.swap(index, target);
    true
}

/// `ids` in their original order with repeats dropped.
pub fn dedup_in_order(ids: impl IntoIterator<Item = MailboxId>) -> Vec<MailboxId> {
    let mut out: Vec<MailboxId> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// `ids` with every id under the renamed folder `from` re-keyed to sit under
/// `to`, order kept and duplicates dropped.
pub fn rekey_ids(ids: &[MailboxId], from: &MailboxId, to: &MailboxId, delimiter: char) -> Vec<MailboxId> {
    dedup_in_order(ids.iter().map(|id| rekey_mailbox_id(id, from, to, delimiter).unwrap_or_else(|| id.clone())))
}

/// The CSS class that tints a folder icon `hex` (one of [`CALENDAR_PALETTE`]).
pub fn color_class(hex: &str) -> String {
    format!("folder-color-{}", hex.trim_start_matches('#'))
}

/// The stylesheet rules for every palette colour: the icon tint
/// ([`color_class`]) and the menu's swatch buttons. Installed once with the
/// folder pane's other CSS.
pub fn palette_css() -> String {
    CALENDAR_PALETTE
        .iter()
        .map(|hex| {
            let bare = hex.trim_start_matches('#');
            format!(".folder-color-{bare} {{ color: {hex}; }}\n.folder-swatch-{bare} {{ background-color: {hex}; }}\n")
        })
        .collect()
}

/// One row's menu: the button, its action group (inserted on the row as
/// `folder`), the heading label and the colour swatches - everything `bind`
/// refreshes for whichever folder the row shows now.
#[derive(Clone)]
pub struct RowMenu {
    pub button: gtk::MenuButton,
    pub actions: gio::SimpleActionGroup,
    info: gtk::Label,
    swatches: Vec<(String, gtk::Button)>,
}

/// Builds a row's menu. `target` is read at activation time (the row slot
/// `bind` writes), and every action goes through `handler`.
pub fn build_row_menu(target: Rc<RefCell<Option<Mailbox>>>, handler: FolderMenuHandler) -> RowMenu {
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More options")
        .css_classes(["hover-quick-action"])
        .valign(gtk::Align::Center)
        .build();
    let actions = gio::SimpleActionGroup::new();
    let dispatch: Rc<dyn Fn(FolderMenuAction)> = {
        let button = button.clone();
        Rc::new(move |action| {
            let mailbox = target.borrow().clone();
            let run = handler.borrow().clone();
            if let (Some(mailbox), Some(run)) = (mailbox, run) {
                run(action, mailbox, button.clone().upcast());
            }
        })
    };
    let simple = [
        ("create", FolderMenuAction::CreateSubfolder),
        ("rename", FolderMenuAction::Rename),
        ("move", FolderMenuAction::Move),
        ("delete", FolderMenuAction::Delete),
        ("empty", FolderMenuAction::Empty),
        ("mark-read", FolderMenuAction::MarkAllRead),
        ("favorite-add", FolderMenuAction::AddFavorite),
        ("favorite-remove", FolderMenuAction::RemoveFavorite),
        ("favorite-up", FolderMenuAction::MoveFavoriteUp),
        ("favorite-down", FolderMenuAction::MoveFavoriteDown),
    ];
    for (name, action) in simple {
        let simple_action = gio::SimpleAction::new(name, None);
        let dispatch = dispatch.clone();
        simple_action.connect_activate(move |_, _| dispatch(action.clone()));
        actions.add_action(&simple_action);
    }
    let color = gio::SimpleAction::new("color", Some(glib::VariantTy::STRING));
    {
        let dispatch = dispatch.clone();
        color.connect_activate(move |_, param| {
            let hex = param.and_then(|p| p.get::<String>()).filter(|hex| !hex.is_empty());
            dispatch(FolderMenuAction::SetColor(hex));
        });
    }
    actions.add_action(&color);
    // Stateful so the two count items render as radio buttons; `bind` sets
    // the state to the row's current mode, and activation only reports the
    // choice - the rebuild that follows sets the state from the stored prefs.
    let count = gio::SimpleAction::new_stateful("count", Some(glib::VariantTy::STRING), &"unread".to_variant());
    {
        let dispatch = dispatch.clone();
        count.connect_activate(move |_, param| {
            let mode = match param.and_then(|p| p.get::<String>()).as_deref() {
                Some("total") => FolderCountMode::Total,
                _ => FolderCountMode::Unread,
            };
            dispatch(FolderMenuAction::SetCountMode(mode));
        });
    }
    actions.add_action(&count);

    let menu = gio::Menu::new();
    let custom_item = |id: &str| {
        let item = gio::MenuItem::new(None, None);
        item.set_attribute_value("custom", Some(&id.to_variant()));
        item
    };
    let heading = gio::Menu::new();
    heading.append_item(&custom_item("info"));
    menu.append_section(None, &heading);

    let create = gio::Menu::new();
    create.append(Some("Create new subfolder"), Some("folder.create"));
    menu.append_section(None, &create);

    let appearance = gio::Menu::new();
    appearance.append(Some("Rename"), Some("folder.rename"));
    let colors = gio::Menu::new();
    colors.append_item(&custom_item("colors"));
    let no_color = gio::MenuItem::new(Some("No color"), None);
    no_color.set_action_and_target_value(Some("folder.color"), Some(&"".to_variant()));
    colors.append_item(&no_color);
    appearance.append_submenu(Some("Change color"), &colors);
    let counts = gio::Menu::new();
    counts.append(Some("Show unread count"), Some("folder.count::unread"));
    counts.append(Some("Show total count"), Some("folder.count::total"));
    appearance.append_submenu(Some("Change folder count"), &counts);
    menu.append_section(None, &appearance);

    let manage = gio::Menu::new();
    manage.append(Some("Move…"), Some("folder.move"));
    manage.append(Some("Delete"), Some("folder.delete"));
    manage.append(Some("Empty"), Some("folder.empty"));
    manage.append(Some("Mark all as read"), Some("folder.mark-read"));
    menu.append_section(None, &manage);

    let favorites = gio::Menu::new();
    for (label, action) in [("Add to Favorites", "folder.favorite-add"), ("Remove from Favorites", "folder.favorite-remove")] {
        // Only one of the pair is ever enabled, and the other is hidden
        // rather than greyed out - the item reads as a single toggle.
        let item = gio::MenuItem::new(Some(label), Some(action));
        item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
        favorites.append_item(&item);
    }
    favorites.append(Some("Move up"), Some("folder.favorite-up"));
    favorites.append(Some("Move down"), Some("folder.favorite-down"));
    menu.append_section(None, &favorites);

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    let info = gtk::Label::builder()
        .xalign(0.0)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .css_classes(["dim-label", "caption"])
        .build();
    popover.add_child(&info, "info");
    let swatch_grid = gtk::Grid::builder()
        .column_spacing(6)
        .row_spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let mut swatches = Vec::with_capacity(CALENDAR_PALETTE.len());
    for (index, hex) in CALENDAR_PALETTE.iter().enumerate() {
        let swatch = gtk::Button::builder()
            .css_classes(["folder-color-swatch", &format!("folder-swatch-{}", hex.trim_start_matches('#'))])
            .tooltip_text(*hex)
            .build();
        {
            let dispatch = dispatch.clone();
            let popover = popover.clone();
            let hex = hex.to_string();
            swatch.connect_clicked(move |_| {
                popover.popdown();
                dispatch(FolderMenuAction::SetColor(Some(hex.clone())));
            });
        }
        swatch_grid.attach(&swatch, (index % 5) as i32, (index / 5) as i32, 1, 1);
        swatches.push((hex.to_string(), swatch));
    }
    popover.add_child(&swatch_grid, "colors");
    button.set_popover(Some(&popover));
    button.insert_action_group("folder", Some(&actions));

    RowMenu { button, actions, info, swatches }
}

impl RowMenu {
    /// Points the menu at `mailbox`: its heading, which items are enabled,
    /// and the checked colour and count mode.
    pub fn refresh(&self, mailbox: &Mailbox, state: FolderMenuState, color: Option<&str>, count_mode: FolderCountMode) {
        self.info.set_label(&folder_info_text(mailbox));
        let enable = |name: &str, enabled: bool| {
            if let Some(action) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(enabled);
            }
        };
        enable("create", state.create);
        enable("rename", state.rename);
        enable("move", state.move_folder);
        enable("delete", state.delete);
        enable("empty", state.empty);
        enable("mark-read", state.mark_read);
        enable("favorite-add", !state.is_favorite);
        enable("favorite-remove", state.is_favorite);
        enable("favorite-up", state.move_up);
        enable("favorite-down", state.move_down);
        if let Some(action) = self.actions.lookup_action("count").and_downcast::<gio::SimpleAction>() {
            let mode = match count_mode {
                FolderCountMode::Unread => "unread",
                FolderCountMode::Total => "total",
            };
            action.set_state(&mode.to_variant());
        }
        for (hex, swatch) in &self.swatches {
            if Some(hex.as_str()) == color {
                swatch.add_css_class("selected");
            } else {
                swatch.remove_css_class("selected");
            }
        }
    }
}

/// Asks for a folder name: "Create new subfolder" (empty `initial`) and
/// "Rename" (the current name). The confirm button stays disabled - with the
/// reason shown under the entry - while [`folder_name_problem`] objects, and
/// `on_confirm` gets the trimmed name.
pub fn present_name_dialog(parent: &gtk::Widget, heading: &str, confirm_label: &str, initial: &str, delimiter: char, siblings: Vec<String>, on_confirm: impl Fn(String) + 'static) {
    let entry = gtk::Entry::builder().text(initial).activates_default(true).build();
    let problem_label = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["caption", "error"]).visible(false).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&entry);
    content.append(&problem_label);
    let dialog = adw::AlertDialog::builder().heading(heading).default_response("confirm").close_response("cancel").build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", confirm_label);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&content));
    let current = (!initial.is_empty()).then(|| initial.to_string());
    let validate = {
        let dialog = dialog.clone();
        let problem_label = problem_label.clone();
        move |text: &str| {
            let problem = folder_name_problem(text, delimiter, &siblings, current.as_deref());
            let unchanged = current.as_deref().is_some_and(|current| current == text.trim());
            dialog.set_response_enabled("confirm", problem.is_none() && !unchanged);
            let message = problem.unwrap_or_default();
            problem_label.set_visible(!message.is_empty());
            problem_label.set_label(&message);
        }
    };
    validate(initial);
    entry.connect_changed(move |entry| validate(&entry.text()));
    {
        let entry = entry.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "confirm" {
                on_confirm(entry.text().trim().to_string());
            }
        });
    }
    dialog.present(Some(parent));
    entry.grab_focus();
}

/// Asks where to move `mailbox`: every other folder of its account that can
/// hold subfolders (not the folder itself or anything beneath it, and not
/// its current parent, where it already is), indented by depth, plus "Top
/// level". `on_pick` gets the chosen parent, `None` for the top level.
pub fn present_move_dialog(parent: &gtk::Widget, mailbox: &Mailbox, folders: Vec<Mailbox>, on_pick: impl Fn(Option<MailboxId>) + 'static) {
    let account_id: AccountId = mailbox.account_id.clone();
    let current_parent = parent_mailbox_id(&account_id, &mailbox.id, mailbox.delimiter);
    let mut rows: Vec<(Option<MailboxId>, String, usize)> = Vec::new();
    if current_parent.is_some() {
        rows.push((None, "Top level".to_string(), 0));
    }
    fn walk(nodes: &[Rc<crate::folder_tree::FolderNode>], depth: usize, out: &mut Vec<(Mailbox, usize)>) {
        for node in nodes {
            out.push((node.mailbox.clone(), depth));
            walk(&node.children, depth + 1, out);
        }
    }
    let mut ordered = Vec::new();
    walk(&crate::folder_tree::build_folder_roots(folders, &account_id), 0, &mut ordered);
    for (folder, depth) in ordered {
        let excluded = is_same_or_descendant(&folder.id, &mailbox.id, mailbox.delimiter)
            || Some(&folder.id) == current_parent.as_ref()
            || folder.flags.iter().any(|flag| flag.eq_ignore_ascii_case("NoInferiors"));
        if !excluded {
            rows.push((Some(folder.id.clone()), display_name(&folder.name), depth));
        }
    }

    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).css_classes(["boxed-list"]).build();
    for (_, label, depth) in &rows {
        let label = gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .margin_start(12 + 16 * *depth as i32)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(8)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        list.append(&label);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(240)
        .max_content_height(360)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Move \u{201c}{}\u{201d}", display_name(&mailbox.name)))
        .body("Choose the folder to move it into.")
        .default_response("move")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("move", "Move");
    dialog.set_response_appearance("move", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("move", false);
    dialog.set_extra_child(Some(&scroller));
    {
        let dialog = dialog.clone();
        list.connect_row_selected(move |_, row| dialog.set_response_enabled("move", row.is_some()));
    }
    dialog.connect_response(None, move |_, response| {
        if response != "move" {
            return;
        }
        let Some(row) = list.selected_row() else { return };
        if let Some((target, _, _)) = rows.get(row.index() as usize) {
            on_pick(target.clone());
        }
    });
    dialog.present(Some(parent));
}

/// Confirms a folder delete. `permanent` is a folder already inside Trash
/// (or an account with no Trash): it is deleted for good, so the dialog says
/// so and the button is destructive; otherwise it is moved under Trash.
pub fn present_delete_dialog(parent: &gtk::Widget, mailbox: &Mailbox, permanent: bool, on_confirm: impl Fn() + 'static) {
    let name = display_name(&mailbox.name);
    let (heading, body, label) = if permanent {
        (
            format!("Permanently delete \u{201c}{name}\u{201d}?"),
            "The folder, its subfolders and every message in them will be permanently deleted. This cannot be undone.",
            "Delete",
        )
    } else {
        (
            format!("Move \u{201c}{name}\u{201d} to Trash?"),
            "The folder and its subfolders will be moved into Trash, where they can still be recovered.",
            "Move to Trash",
        )
    };
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", label);
    dialog.set_response_appearance(
        "delete",
        if permanent {
            adw::ResponseAppearance::Destructive
        } else {
            adw::ResponseAppearance::Suggested
        },
    );
    dialog.connect_response(None, move |_, response| {
        if response == "delete" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}

/// Confirms emptying a Trash or Junk folder - irreversible, so it always
/// asks first. Shared by the row's hover Empty button and the menu item.
pub fn present_empty_dialog(parent: &gtk::Widget, mailbox: &Mailbox, on_confirm: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Empty {}?", display_name(&mailbox.name)))
        .body("All messages in this folder will be permanently deleted. This cannot be undone.")
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("empty", "Empty");
    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
    dialog.connect_response(None, move |_, response| {
        if response == "empty" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::UidValidity;

    fn folder(path: &str, role: MailboxRole, unread: u32, flags: &[&str]) -> Mailbox {
        let account = AccountId("acc".into());
        Mailbox {
            id: MailboxId::new(&account, path),
            account_id: account,
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            parent: None,
            delimiter: '/',
            role,
            uidvalidity: UidValidity(1),
            uidnext: 1,
            highest_modseq: None,
            total: 35,
            unread,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            subscribed: true,
        }
    }

    #[test]
    fn special_folders_lock_rename_move_and_delete() {
        let inbox = folder_menu_state(&folder("INBOX", MailboxRole::Inbox, 0, &[]), Some((0, 3)));
        assert!(inbox.create && !inbox.rename && !inbox.move_folder && !inbox.delete);
        assert!(!inbox.empty, "Empty is the Trash/Junk expunge");
        assert!(!inbox.mark_read, "nothing unread");
        assert!(inbox.is_favorite && !inbox.move_up && inbox.move_down, "first of three favourites");

        let custom = folder_menu_state(&folder("Work", MailboxRole::Custom, 4, &["NoInferiors"]), None);
        assert!(custom.rename && custom.move_folder && custom.delete && custom.mark_read);
        assert!(!custom.create, "\\Noinferiors can't hold subfolders");
        assert!(!custom.is_favorite && !custom.move_up && !custom.move_down);

        let trash = folder_menu_state(&folder("Trash", MailboxRole::Trash, 0, &[]), Some((2, 3)));
        assert!(trash.empty && !trash.delete);
        assert!(trash.move_up && !trash.move_down, "last favourite");
    }

    #[test]
    fn info_text_counts_items_and_unread() {
        assert_eq!(folder_info_text(&folder("INBOX", MailboxRole::Inbox, 0, &[])), "35 items (0 unread)");
        let mut one = folder("Work", MailboxRole::Custom, 1, &[]);
        one.total = 1;
        assert_eq!(folder_info_text(&one), "1 item (1 unread)");
    }

    #[test]
    fn folder_names_are_validated_against_siblings() {
        let siblings = vec!["Reports".to_string(), "2026".to_string()];
        assert_eq!(folder_name_problem("  ", '/', &siblings, None), Some(String::new()), "empty: disabled, no message");
        assert!(folder_name_problem("a/b", '/', &siblings, None).is_some());
        assert!(folder_name_problem("reports", '/', &siblings, None).is_some(), "case-insensitive duplicate");
        assert_eq!(folder_name_problem("Invoices", '/', &siblings, None), None);
        assert_eq!(folder_name_problem("REPORTS", '/', &siblings, Some("Reports")), None, "re-casing its own name is fine");
    }

    #[test]
    fn favourites_move_within_bounds() {
        let ids: Vec<MailboxId> = ["a", "b", "c"].iter().map(|s| MailboxId(s.to_string())).collect();
        let mut order = ids.clone();
        assert!(move_in_order(&mut order, &ids[1], -1));
        assert_eq!(order, vec![ids[1].clone(), ids[0].clone(), ids[2].clone()]);
        assert!(!move_in_order(&mut order, &ids[1], -1), "already first");
        assert!(!move_in_order(&mut order, &ids[2], 1), "already last");
        assert!(!move_in_order(&mut order, &MailboxId("z".into()), 1), "not a favourite");
    }

    #[test]
    fn rekeying_follows_a_renamed_subtree() {
        let id = |s: &str| MailboxId(format!("acc:{s}"));
        let ids = vec![id("Work"), id("Work/2026"), id("Work2"), id("Archive/Work")];
        let rekeyed = rekey_ids(&ids, &id("Work"), &id("Archive/Work"), '/');
        assert_eq!(
            rekeyed,
            vec![id("Archive/Work"), id("Archive/Work/2026"), id("Work2")],
            "the collision collapses to one entry"
        );
    }

    #[test]
    fn palette_css_covers_every_colour() {
        let css = palette_css();
        for hex in CALENDAR_PALETTE {
            assert!(css.contains(&format!(".{} {{ color: {hex}; }}", color_class(hex))));
        }
    }
}
