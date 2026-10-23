/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use chrono::Timelike;
use gtk::{gio, glib};
use lookout_core::calendar_protocol::CalendarCommand;
use lookout_core::{display_name, AccountId, CalendarTask, EmailSummary, EventOccurrence, Mailbox, MailboxId, MailboxRole, SystemFlagBit, Uid};
use lookout_dav::subscription::SubscriptionCommand;
use lookout_mail::session::AccountCommand;
use lookout_mail::SearchScope;
use webkit::prelude::*;

use crate::calendar_colors;
use crate::calendar_view::{self};
use crate::contacts_view::{
    contact_identity, delete_contact_entries, export_current_contacts, find_contact_by_address, microsoft_graph_contacts_account, rebuild_contacts_list_ui,
    refresh_contacts_category_ui, show_contact_details_dialog, show_contact_editor_for, show_contacts_import_dialog, show_create_contact_for, show_manage_groups_dialog,
    show_new_contact_editor, spawn_contacts_discovery, sync_contacts_account, ContactCommand, ContactFileFormat, ContactsCategoryChoice, ContactsListEntry,
};
use crate::folder_tree::TreeItem;
use crate::google_tasks::GoogleTasksCommand;
use crate::last_view::{self};
use crate::message_list::{format_row_date, Filter, FilterSet, MessageItem, MessageListModel, ReadingTarget, SelectionKind, SortKey, ThreadId};
use crate::ui_state_db::UiStateDb;
use crate::util::card_section;
use crate::worker::Worker;

// The shell is split by concern: each submodule owns one slice of the
// behaviour `build_window` wires up. Every module is re-exported here so
// the submodules can refer to each other's items unqualified.
mod attachments;
mod calendar_glue;
mod calendar_search;
mod compose_host;
mod connect;
mod conversation;
mod drag_files;
mod flags;
mod folders;
mod graph_sync;
mod imip;
mod mail_actions;
mod optimistic;
mod reading;
mod search;
mod state;
mod support;
mod undo;

#[cfg(test)]
mod tests;

pub(crate) use attachments::*;
pub(crate) use calendar_glue::*;
pub(crate) use calendar_search::*;
pub(crate) use compose_host::*;
pub(crate) use connect::*;
pub(crate) use conversation::*;
pub(crate) use drag_files::*;
pub(crate) use flags::*;
pub(crate) use folders::*;
pub(crate) use graph_sync::*;
pub(crate) use imip::*;
pub(crate) use mail_actions::*;
pub(crate) use optimistic::*;
pub(crate) use reading::*;
pub(crate) use search::*;
pub(crate) use state::*;
pub(crate) use support::*;
pub(crate) use undo::*;

pub fn build_window(app: &adw::Application, worker: Rc<Worker>) -> adw::ApplicationWindow {
    // The theme stack (base palette + bundled flat-token theme + custom
    // accent) registers before any other CSS provider so the app's rules can
    // reference its `lookout-*` tokens. Seeded from GSettings when the Config
    // rows are wired further down.
    let theme_manager = crate::theme::ThemeManager::install();
    crate::style::install();
    // Privacy mode's blur provider (Config → Advanced → "Blur sensitive
    // information"). Installed disabled; seeded from GSettings when the
    // Advanced rows are wired further down.
    let privacy_mode = crate::privacy::PrivacyMode::install();

    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| app.quit()
    });
    app.add_action(&quit_action);

    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(|_, _| show_about_dialog());
    app.add_action(&about_action);

    // Phase 5: the process-wide preference store, resolved once up front
    // (GSettings-backed when the schema is available, session-only memory
    // otherwise) and handed everywhere preferences are read or written -
    // including `background_image`/`last_view`, which need it before
    // `UiState` exists. One-time imports of the pre-GSettings plain files
    // run before the first reads below.
    let settings = Rc::new(crate::settings::resolve());
    crate::last_view::migrate_legacy(&settings);
    crate::background_image::migrate_legacy(&settings);
    // The optional Fluent icon pack (UX-80): applied before any widget is
    // built, so a window that starts in Fluent mode never flashes GNOME
    // glyphs first. The store is shared with the View ribbon's Icons menu and
    // Config → Appearance, both wired below.
    let icon_style = crate::icon_style::IconStyleState::new(crate::icon_style::IconStyle::from_code(&settings.get_string(crate::settings::ICON_STYLE)));

    // --- Background portal approval: ask the session's portal once (the
    // shell shows a dialog the first time, then remembers) so Lookout is
    // listed among the desktop's background apps and can be stopped there,
    // whenever the close-to-background feature is on. Fire-and-forget: no
    // portal (or a denial) only means the app is invisible to that listing,
    // not that background running stops.
    if settings.get_bool(crate::settings::CLOSE_TO_BACKGROUND) {
        worker.spawn(async { crate::background::request_background_approval().await });
    }

    let bg_bytes = crate::resources::bytes("/io/github/gavindi/Lookout/backgrounds/background2.jpg")
        .unwrap_or_else(|| glib::Bytes::from_static(include_bytes!("../../../data/resources/backgrounds/background2.jpg")));
    let default_bg_texture = gtk::gdk::Texture::from_bytes(&bg_bytes).expect("bundled background image should decode");
    let background = gtk::Picture::for_paintable(&default_bg_texture);
    background.set_content_fit(gtk::ContentFit::Cover);
    background.set_can_shrink(true);
    background.set_hexpand(true);
    background.set_vexpand(true);
    // A custom background chosen under Config → Appearance → "Window
    // background" wins over the bundled artwork when it's still around and
    // still decodes; the Config view rows are told about it further down.
    let custom_background_name = crate::background_image::load(&settings).and_then(|path| match gtk::gdk::Texture::from_filename(&path) {
        Ok(texture) => {
            background.set_paintable(Some(&texture));
            path.file_name().map(|name| name.to_string_lossy().into_owned())
        }
        Err(_) => {
            tracing::warn!(path = %path.display(), "ignoring unreadable custom background image");
            None
        }
    });

    let toast_overlay = adw::ToastOverlay::new();

    let status_page = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("No Mail Accounts")
        .description("Add an account with Mail enabled in GNOME Online Accounts to get started.")
        .build();
    let open_settings_button = gtk::Button::builder()
        .label("Open Online Accounts Settings")
        .halign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    open_settings_button.connect_clicked({
        let worker = worker.clone();
        move |_| worker.spawn(crate::online_accounts::open_online_accounts())
    });
    // The first-run route for Evolution users (EDS_TODO.md E3.4); hidden
    // until the background check finds an account to import.
    let import_evolution_button = gtk::Button::builder()
        .label("Import from Evolution…")
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .visible(false)
        .build();
    let status_buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).halign(gtk::Align::Center).build();
    status_buttons.append(&open_settings_button);
    status_buttons.append(&import_evolution_button);
    status_page.set_child(Some(&status_buttons));

    // --- Folder sidebar: one expanded-by-default group per account ---
    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_factory = gtk::SignalListItemFactory::new();
    // The folder rows are built before `state` exists (it's created further
    // down, once the mail accounts are wired up), so the drop target reaches
    // it through this slot, filled right after `state` is created.
    let state_slot: Rc<RefCell<Option<Rc<RefCell<UiState>>>>> = Rc::new(RefCell::new(None));
    // What the rows' "⋯" menu (and Empty button) actions do - installed by
    // `install_folder_menu_handler` once `state` and the list header exist.
    let menu_handler: crate::folder_menu::FolderMenuHandler = Rc::new(RefCell::new(None));
    {
        let state_slot = state_slot.clone();
        let menu_handler = menu_handler.clone();
        folder_factory.connect_setup(move |_, list_item| {
            let expander = gtk::TreeExpander::new();
            let icon = gtk::Image::builder().icon_size(gtk::IconSize::Normal).build();
            // `hexpand` on the name, not the box, is what pushes the count to the
            // trailing edge: the name takes all the slack (and ellipsizes into it
            // when the pane is narrow) and the count keeps its natural width.
            let label = gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["folder-row-label"])
                .build();
            // Shown only while this row's mailbox is syncing or being
            // prefetched (see `UiState::syncing`/`prefetching` and
            // `refresh_folder_spinner`); sits right of the name and left of
            // the unread count, both of which trail behind it since `label`
            // is the box's only hexpanding child.
            let spinner = gtk::Spinner::builder()
                .visible(false)
                .valign(gtk::Align::Center)
                .width_request(14)
                .height_request(14)
                .build();
            let count = gtk::Label::builder().xalign(1.0).css_classes(["folder-unread-count"]).build();
            let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
            row_box.append(&icon);
            row_box.append(&label);
            row_box.append(&spinner);
            row_box.append(&count);
            expander.set_child(Some(&row_box));
            // Drop target: a folder row accepts the internal message-drag
            // payload and moves the messages into that folder. The target
            // identity (which folder this row shows) is written by `bind`
            // into `drop_data`, since the drop can't read the model itself.
            let drop_data: Rc<RefCell<Option<Mailbox>>> = Rc::new(RefCell::new(None));
            let drop_target = gtk::DropTarget::builder()
                .formats(&gtk::gdk::ContentFormats::for_type(glib::Bytes::static_type()))
                .actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE)
                .build();
            {
                let drop_data = drop_data.clone();
                let state_slot = state_slot.clone();
                drop_target.connect_drop(move |_target, value, _x, _y| {
                    let Some(mailbox) = drop_data.borrow().clone() else {
                        return false;
                    };
                    let Some(state) = state_slot.borrow().clone() else {
                        return false;
                    };
                    handle_message_drag_drop(&state, &mailbox, value)
                });
            }
            {
                let expander = expander.clone();
                drop_target.connect_enter(move |_target, _x, _y| {
                    expander.add_css_class("lookout-drop-target");
                    gtk::gdk::DragAction::MOVE
                });
            }
            {
                let expander = expander.clone();
                drop_target.connect_leave(move |_target| {
                    expander.remove_css_class("lookout-drop-target");
                });
            }
            expander.add_controller(drop_target);
            // Quick actions (initially hidden), mirroring the message rows'
            // hover quick actions: on Trash/Junk a button that empties the
            // folder, and on every folder row the "⋯" menu (see
            // `folder_menu`). `bind` writes which folder the row shows into
            // `menu_data` (and whether it can be emptied into
            // `expunge_enabled`), read at click time - the same slot pattern
            // as the drop target above. Both route through `menu_handler`,
            // installed once the window state exists.
            let expunge_btn = gtk::Button::from_icon_name("user-trash-full-symbolic");
            expunge_btn.add_css_class("hover-quick-action");
            expunge_btn.set_tooltip_text(Some("Empty folder"));
            expunge_btn.set_visible(false);
            let menu_data: Rc<RefCell<Option<Mailbox>>> = Rc::new(RefCell::new(None));
            let menu = crate::folder_menu::build_row_menu(menu_data.clone(), menu_handler.clone());
            let action_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .halign(gtk::Align::End)
                .valign(gtk::Align::Center)
                .margin_end(8)
                .build();
            action_box.add_css_class("hover-quick-actions");
            action_box.append(&expunge_btn);
            action_box.append(&menu.button);
            action_box.set_visible(false);
            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&expander));
            overlay.add_overlay(&action_box);
            let expunge_data: Rc<RefCell<Option<Mailbox>>> = Rc::new(RefCell::new(None));
            let expunge_enabled: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            {
                let expunge_data = expunge_data.clone();
                let menu_handler = menu_handler.clone();
                expunge_btn.connect_clicked(move |button| {
                    let mailbox = expunge_data.borrow().clone();
                    let handler = menu_handler.borrow().clone();
                    if let (Some(mailbox), Some(handler)) = (mailbox, handler) {
                        handler(crate::folder_menu::FolderMenuAction::Empty, mailbox, button.clone().upcast());
                    }
                });
            }
            // The action box shows on a folder row only while the pointer is
            // over it - or while its menu is open, so that moving the pointer
            // from the row into the menu doesn't hide the button the menu
            // hangs off (closing it). Selecting a row doesn't show it.
            let hovered = Rc::new(Cell::new(false));
            let refresh_actions: Rc<dyn Fn()> = {
                let action_box = action_box.clone();
                let menu_data = menu_data.clone();
                let hovered = hovered.clone();
                let menu_button = menu.button.clone();
                Rc::new(move || {
                    let menu_open = menu_button.popover().is_some_and(|popover| popover.is_visible());
                    action_box.set_visible(menu_data.borrow().is_some() && (hovered.get() || menu_open));
                })
            };
            {
                let hovered_enter = hovered.clone();
                let refresh_enter = refresh_actions.clone();
                let refresh_leave = refresh_actions.clone();
                let hover_controller = gtk::EventControllerMotion::new();
                hover_controller.connect_enter(move |_, _, _| {
                    hovered_enter.set(true);
                    refresh_enter();
                });
                hover_controller.connect_leave(move |_| {
                    hovered.set(false);
                    refresh_leave();
                });
                overlay.add_controller(hover_controller);
            }
            if let Some(popover) = menu.button.popover() {
                let refresh = refresh_actions.clone();
                popover.connect_closed(move |_| refresh());
            }
            unsafe {
                expander.set_data(
                    "lookout-expunge-widgets",
                    FolderRowWidgets {
                        expunge_btn,
                        expunge_data,
                        expunge_enabled,
                        menu,
                        menu_data,
                        refresh_actions,
                    },
                );
            }
            unsafe {
                expander.set_data("lookout-drop-target", drop_data);
            }
            list_item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&overlay));
        });
    }
    {
        let state_slot = state_slot.clone();
        folder_factory.connect_bind(move |_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(overlay) = list_item.child().and_downcast::<gtk::Overlay>() else {
                return;
            };
            let Some(expander) = overlay.child().and_downcast::<gtk::TreeExpander>() else {
                return;
            };
            expander.set_list_row(Some(&row));
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let tree_item = boxed.borrow::<TreeItem>();
            let Some(row_box) = expander.child().and_downcast::<gtk::Box>() else { return };
            let Some(icon) = row_box.first_child().and_downcast::<gtk::Image>() else { return };
            let Some(label) = icon.next_sibling().and_downcast::<gtk::Label>() else { return };
            let Some(spinner) = label.next_sibling().and_downcast::<gtk::Spinner>() else { return };
            let Some(count) = row_box.last_child().and_downcast::<gtk::Label>() else { return };
            // A zero count is hidden rather than blanked: an empty label still
            // reserves its spacing, which would leave every quiet folder's name
            // ending short of the ones beside it.
            let set_count = |unread: u32| {
                count.set_visible(unread > 0);
                if unread > 0 {
                    count.set_label(&unread.to_string());
                }
            };
            // A recycled row may carry the previous folder's colour or count
            // mode; only a folder row puts its own back (below).
            icon.set_css_classes(&[]);
            count.set_css_classes(&["folder-unread-count"]);
            // The folder's local display prefs and its place in the Favorites
            // section, for the colour/count below and the "⋯" menu further on.
            let folder_view = match &*tree_item {
                TreeItem::Folder(node) | TreeItem::Favorite(node) => state_slot.borrow().as_ref().map(|state| {
                    let st = state.borrow();
                    let prefs = st.folder_prefs.get(&node.mailbox.id).cloned().unwrap_or_default();
                    let order = &st.favorite_display_order;
                    let favorite_pos = order.iter().position(|m| m == &node.mailbox.id).map(|index| (index, order.len()));
                    // Graph mail has no folder move (item 5.7.6).
                    let folder_move_supported = mailbox_account_id(&node.mailbox.id)
                        .and_then(|account_id| st.accounts.get(&account_id))
                        .is_none_or(|handle| !handle.uses_graph_mail);
                    (prefs, favorite_pos, folder_move_supported)
                }),
                _ => None,
            };
            match &*tree_item {
                TreeItem::Unified(unread) => {
                    icon.set_visible(true);
                    icon.set_icon_name(Some("mail-inbox-symbolic"));
                    label.set_label("All Inboxes");
                    label.set_css_classes(&["heading", "folder-row-label"]);
                    crate::privacy::mark_ui(&label);
                    set_count(*unread);
                }
                TreeItem::Favorites => {
                    icon.set_visible(true);
                    icon.set_icon_name(Some(themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])));
                    label.set_label("Favorites");
                    label.set_css_classes(&["heading", "folder-row-label"]);
                    crate::privacy::mark_ui(&label);
                    // A pure grouping row: every folder under it shows its own.
                    set_count(0);
                }
                TreeItem::Account(account) => {
                    icon.set_visible(false);
                    label.set_label(&account.label);
                    // An account row's label is the account name/address, so it
                    // blurs under privacy mode. Every other arm resets the
                    // classes (dropping this one), so folder names stay sharp.
                    label.set_css_classes(&["heading", "folder-row-label", crate::privacy::SENSITIVE_CLASS]);
                    set_count(account.unread);
                }
                TreeItem::SearchFolders => {
                    icon.set_visible(true);
                    icon.set_icon_name(Some(themed_icon_name(&["edit-find-symbolic", "system-search-symbolic"])));
                    label.set_label("Search Folders");
                    label.set_css_classes(&["heading", "folder-row-label"]);
                    crate::privacy::mark_ui(&label);
                    set_count(0);
                }
                TreeItem::SearchFolder(folder) => {
                    icon.set_visible(true);
                    icon.set_icon_name(Some(themed_icon_name(&["edit-find-symbolic", "system-search-symbolic"])));
                    label.set_label(&folder.name);
                    label.set_css_classes(&["folder-row-label"]);
                    set_count(0);
                }
                // A favorite renders exactly like the folder it duplicates.
                TreeItem::Folder(node) | TreeItem::Favorite(node) => {
                    icon.set_visible(true);
                    icon.set_icon_name(Some(folder_icon_name(node.mailbox.role)));
                    label.set_label(&display_name(&node.mailbox.name));
                    label.set_css_classes(&["folder-row-label"]);
                    let prefs = folder_view.as_ref().map(|(prefs, _, _)| prefs.clone()).unwrap_or_default();
                    if let Some(color) = &prefs.color {
                        icon.add_css_class(&crate::folder_menu::color_class(color));
                    }
                    match prefs.count_mode {
                        crate::ui_state_db::FolderCountMode::Unread => set_count(node.mailbox.unread),
                        crate::ui_state_db::FolderCountMode::Total => {
                            count.add_css_class("folder-count-total");
                            set_count(node.mailbox.total);
                        }
                    }
                }
            }
            // Refresh the row's drag-drop identity: folder rows accept moves
            // into that folder, everything else accepts nothing.
            if let Some(drop_data) = unsafe { expander.data::<Rc<RefCell<Option<Mailbox>>>>("lookout-drop-target") } {
                let mut slot = unsafe { drop_data.as_ref() }.borrow_mut();
                *slot = match &*tree_item {
                    TreeItem::Folder(node) | TreeItem::Favorite(node) => Some(node.mailbox.clone()),
                    _ => None,
                };
            }
            // Refresh the row's quick actions: every folder row gets the "⋯"
            // menu, pointed at this folder; Trash and Junk rows also get the
            // Empty button (with a role-named tooltip). Other rows get none.
            let row_mailbox = match &*tree_item {
                TreeItem::Folder(node) | TreeItem::Favorite(node) => Some(node.mailbox.clone()),
                _ => None,
            };
            if let Some(widgets) = unsafe { expander.data::<FolderRowWidgets>("lookout-expunge-widgets") } {
                let widgets = unsafe { widgets.as_ref() };
                let role = row_mailbox.as_ref().map_or(MailboxRole::Custom, |m| m.role);
                let expunge_eligible = matches!(role, MailboxRole::Trash | MailboxRole::Junk);
                widgets.expunge_enabled.set(expunge_eligible);
                widgets.expunge_btn.set_visible(expunge_eligible);
                *widgets.expunge_data.borrow_mut() = row_mailbox.clone().filter(|_| expunge_eligible);
                widgets.expunge_btn.set_tooltip_text(Some(match role {
                    MailboxRole::Trash => "Empty Trash",
                    MailboxRole::Junk => "Empty Junk",
                    _ => "Empty folder",
                }));
                if let Some(mailbox) = &row_mailbox {
                    let (prefs, favorite_pos, folder_move_supported) = folder_view.clone().unwrap_or_default();
                    let menu_state = crate::folder_menu::folder_menu_state(mailbox, favorite_pos, folder_move_supported);
                    widgets.menu.refresh(mailbox, menu_state, prefs.color.as_deref(), prefs.count_mode);
                }
                *widgets.menu_data.borrow_mut() = row_mailbox;
                (widgets.refresh_actions)();
            }
            // Refresh the row's sync/prefetch spinner: register it under its
            // mailbox id (for `set_mailbox_syncing`/`set_mailbox_prefetching`
            // to reach it live) and stamp which mailbox it now represents, so
            // a toggle arriving for a mailbox this row was recycled away from
            // (see `folder_row_spinners`'s doc comment) is recognized as
            // stale and skipped.
            let spinner_mailbox = match &*tree_item {
                TreeItem::Folder(node) | TreeItem::Favorite(node) => Some(node.mailbox.id.clone()),
                _ => None,
            };
            unsafe {
                spinner.set_data("lookout-sync-mailbox", spinner_mailbox.clone());
            }
            if let (Some(mailbox_id), Some(state)) = (&spinner_mailbox, state_slot.borrow().clone()) {
                let mut st = state.borrow_mut();
                let entry = st.folder_row_spinners.entry(mailbox_id.clone()).or_default();
                // Drop dead weak refs and any existing entry for this exact
                // widget (a rebind of the same row) before re-adding it, so a
                // row that toggles between mailboxes doesn't accumulate
                // duplicate entries for the mailboxes it's passed through.
                entry.retain(|weak| weak.upgrade().is_some_and(|s| s != spinner));
                entry.push(spinner.downgrade());
                let active = st.syncing.contains(mailbox_id) || st.prefetching.contains(mailbox_id);
                spinner.set_visible(active);
                spinner.set_spinning(active);
            } else {
                spinner.set_visible(false);
                spinner.set_spinning(false);
            }
        });
    }

    let folder_list_view = gtk::ListView::new(Some(folder_selection.clone()), Some(folder_factory));
    let folder_scroller = gtk::ScrolledWindow::builder().child(&folder_list_view).vexpand(true).build();
    let folder_card = card_section(&folder_scroller);
    folder_card.add_css_class("folder-pane");
    folder_card.add_css_class("card-flush-end");
    folder_card.add_css_class(crate::settings::spacing_class(&settings.get_string(crate::settings::LAYOUT_SPACING)));
    folder_card.set_margin_end(0);
    // Floor on the folder pane's width, so the separator can't be dragged
    // left until the folder names are a sliver. Set on the card rather than
    // on `folder_scroller`'s child because `Gtk.ScrolledWindow` absorbs its
    // child's size request instead of propagating it - the same reason
    // `reading_stack`'s height floor is set where it is. It only bites
    // because `main_paned` sets `shrink_start_child(false)`, which is what
    // makes the Paned honour the child's minimum.
    folder_card.set_size_request(FOLDER_PANE_MIN_WIDTH, -1);

    // --- Message list ---
    let message_list = MessageListModel::build_with_worker(worker.clone());
    let last_selection = last_view::load(&settings);
    // Phase 5: the scalars that used to be hardcoded here now come from the
    // GSettings-backed store (see `settings`), so the session starts where
    // the last one ended. The store falls back to its schema defaults when
    // no schema is available, which is exactly these old values.
    let ui_db = crate::ui_state_db::UiStateDb::open()
        .map(Rc::new)
        .inspect_err(|e| tracing::warn!("starred contacts and sender trust/block lists won't persist: {e}"))
        .ok();
    let starred_contacts: HashSet<(AccountId, String)> = ui_db
        .as_ref()
        .and_then(|db| db.load_starred().ok())
        .into_iter()
        .flat_map(|by_account| {
            by_account
                .into_iter()
                .flat_map(|(account, identities)| identities.into_iter().map(move |identity| (account.clone(), identity)))
        })
        .collect();
    let trusted_senders: HashMap<(AccountId, String), lookout_core::TrustLevel> = ui_db
        .as_ref()
        .and_then(|db| db.load_trusted_senders().ok())
        .into_iter()
        .flatten()
        .map(|(account, entry, level)| ((account, entry), level))
        .collect();
    // The blocked-sender list (item 2.9 / M-17): addressed entered from the
    // message menu's Block sender / Block domain. Enforced by the client rule
    // path on arrival; the load-policy's safe list reuses `trusted_senders`.
    let blocked_senders: HashSet<(AccountId, String)> = ui_db.as_ref().and_then(|db| db.load_blocked_senders().ok()).into_iter().flatten().collect();
    // Ignored conversations (item 2.8 / M-20): thread keys entered from the
    // message menu's Ignore; new mail in one is moved to Deleted on arrival.
    let ignored_threads: HashSet<(AccountId, String)> = ui_db
        .as_ref()
        .and_then(|db| db.load_ignored_threads().ok())
        .into_iter()
        .flatten()
        .map(|(account, thread, _label)| (account, thread))
        .collect();
    // The folder pane's expansion memory: which subfolders the user left
    // expanded and which account groups collapsed. Loaded from the same
    // UI-state database so the first `rebuild_folder_tree` of a fresh session
    // can re-seed the pane's collapse state instead of starting all-default.
    let (expanded_folders, collapsed_groups) = ui_db.as_ref().and_then(|db| db.load_folder_expansion().ok()).unwrap_or_default();
    let folder_prefs = ui_db.as_ref().and_then(|db| db.load_folder_prefs().ok()).unwrap_or_default();
    let recent_folders = ui_db.as_ref().and_then(|db| db.load_recent_folders().ok()).unwrap_or_default();
    let recent_searches = ui_db.as_ref().and_then(|db| db.load_recent_searches().ok()).unwrap_or_default();
    // Shared keyring handle for manually-added ("other") IMAP/SMTP accounts -
    // see `other_accounts.rs`. Cloned into the add-account dialog, the
    // startup connect loop, and the Config view's edit/remove actions.
    let keyring = crate::other_accounts::SecretServiceKeyring::new();
    let state = Rc::new(RefCell::new(UiState {
        accounts: HashMap::new(),
        contacts_by_account: HashMap::new(),
        starred_contacts,
        contacts_filter: String::new(),
        contacts_refresh: None,
        contacts_list_refresh: None,
        contacts_filter_options: crate::contacts_view::ContactsFilterOptions::default(),
        contacts_details_refresh: None,
        desktop_contacts: Vec::new(),
        ui_db,
        settings: settings.clone(),
        app_config: Rc::new(RefCell::new(crate::app_config::load())),
        categories: Rc::new(RefCell::new(crate::categories::CategorySet::default())),
        deleted_contacts: HashMap::new(),
        contact_cmd_tx: HashMap::new(),
        goa_accounts: HashMap::new(),
        goa_client: None,
        goa_unavailable: false,
        app: Some(app.clone()),
        current_account: None,
        current_mailbox: None,
        pending_message_selection: None,
        mail_view: MailView::Single,
        unified_snapshots: HashMap::new(),
        mailbox_sync_epoch: HashMap::new(),
        pending_optimistic_removals: HashMap::new(),
        pending_optimistic_flag_changes: HashMap::new(),
        pending_optimistic_pinned_changes: HashMap::new(),
        pending_body_request: None,
        body_fetch_error: None,
        body_fetch_generation: 0,
        pending_attachment: None,
        pending_attachment_batch: None,
        pending_raw_message: None,
        pending_report_phishing: None,
        pending_rule_forwards: Vec::new(),
        pending_rule_evals: Vec::new(),
        message_windows: HashMap::new(),
        message_details: None,
        rendered_draft: None,
        rendered_draft_message_id: None,
        rendered_snooze: None,
        rendered_follow_up: None,
        flagged_mail: HashMap::new(),
        outbox: HashMap::new(),
        outbox_banner: None,
        outbox_dialog: None,
        sieve_support: HashMap::new(),
        automatic_replies: HashMap::new(),
        automatic_replies_banner: None,
        automatic_replies_dialog_refresh: None,
        pending_draft_open: None,
        pending_forward_raw: None,
        unsubscribe_info: None,
        unsubscribe_dismissed: None,
        imip: None,
        imip_dismissed: None,
        imip_replies_applied: HashSet::new(),
        calendar_state: None,
        read_receipt_request: None,
        read_receipt_dismissed: None,
        read_receipts_sent: HashSet::new(),
        read_receipt_context: None,
        pending_cid: HashMap::new(),
        rendered_conversation: None,
        conversation_inline_parts: HashMap::new(),
        conversation_ui: None,
        conversation_mark_on_leave: Vec::new(),
        rendered_inline_parts: Vec::new(),
        temp_attachment_files: HashSet::new(),
        toast_overlay: Some(toast_overlay.clone()),
        pending_sends: Vec::new(),
        next_send_token: 1,
        undo_stack: Vec::new(),
        next_undo_id: 0,
        pending_html_reveal: false,
        pending_header: None,
        body_cache: BodyCache::new(BODY_CACHE_IN_MEMORY),
        reveal_generation: 0,
        last_selection: last_selection.clone(),
        restore_pending: last_selection.is_some(),
        rendered_message: None,
        pending_mark_read: None,
        mark_read_on_leave: None,
        syncing: HashSet::new(),
        prefetching: HashSet::new(),
        folder_row_spinners: HashMap::new(),
        refreshing: HashSet::new(),
        graph_property_syncs: Default::default(),
        sort_key: SortKey::from_action_state(&settings.get_string(crate::settings::SORT_KEY)).unwrap_or(SortKey::Date),
        sort_descending: settings.get_bool(crate::settings::SORT_DESCENDING),
        favorites: crate::folder_menu::dedup_in_order(settings.get_strv(crate::settings::MAIL_FAVORITES).into_iter().map(MailboxId)),
        favorite_display_order: Vec::new(),
        folder_prefs,
        recent_folders,
        recent_searches,
        search_scope: SearchScope::from_setting(&settings.get_string(crate::settings::MAIL_SEARCH_SCOPE)),
        body_waiter: None,
        load_remote_images: settings.get_bool(crate::settings::MAIL_LOAD_REMOTE_IMAGES),
        rich_text_default: settings.get_bool(crate::settings::MAIL_RICH_TEXT_DEFAULT),
        trusted_senders,
        blocked_senders,
        ignored_threads,
        folder_tree_refresh: None,
        rendered_trust_sender: None,
        rendered_from_mismatch: None,
        rendered_link_warnings: Vec::new(),
        load_once_images: false,
        message_theme_override: false,
        trust_banner_dismissed: None,
        rendered_remote_scan: None,
        draft_saved_tx: None,
        composer_identities_refresh: None,
        manage_signatures: None,
        manage_templates: None,
        create_task_from_message: None,
        reply_with_meeting: None,
        flagged_mail_refresh: None,
        composer_relay_generation: 0,
        compose_popout_window: None,
        compose_action_bar: None,
        folder_tree: None,
        expanded_folders,
        collapsed_groups,
        suppress_folder_selection: false,
        search_active: false,
        search_query: String::new(),
        search_results: Vec::new(),
        search_pending: HashSet::new(),
        search_entry: None,
        search_ribbon: None,
        search_ribbon_prev: None,
        ribbon_style: None,
    }));
    // The ribbon's shared layout state: the Simplified/Classic mode and every
    // live band, so one switch reaches the main window's ribbon and any open
    // composer's toolbar. Read from GSettings at startup, written back by the
    // layout menu and the Config → Appearance row (`ribbon.rs`).
    let ribbon_style = crate::ribbon::RibbonStyle::new(crate::ribbon::RibbonMode::from_code(&settings.get_string(crate::settings::RIBBON_STYLE)));
    state.borrow_mut().ribbon_style = Some(ribbon_style.clone());
    // The layout switch: a stateful `win.ribbon-style` action carrying the
    // mode's code, driven by the ribbon's own "Ribbon layout" menu and by
    // Config → Appearance's "Ribbon style" row. Its activation handler is
    // wired below, once the Config row exists.
    let ribbon_style_action = gio::SimpleAction::new_stateful("ribbon-style", Some(glib::VariantTy::STRING), &ribbon_style.mode().code().to_variant());
    // The pack switch: a stateful `win.icon-style` action in the same shape,
    // driven by View ▸ Icons and Config → Appearance's "Icon style" row.
    let icon_style_action = gio::SimpleAction::new_stateful("icon-style", Some(glib::VariantTy::STRING), &icon_style.style().code().to_variant());
    // The folder rows' drop targets reach `state` through this slot (they
    // were built before it existed).
    *state_slot.borrow_mut() = Some(state.clone());
    // Clean up the temporary files materialized for the attachment strip's
    // "Open" action when the app exits - the viewer process may still be
    // running, but Lookout must not leak temp files. (On Flatpak the portal
    // makes its own copy for the viewer, so removing ours is always safe.)
    let state_for_shutdown = state.clone();
    app.connect_shutdown(move |_| {
        let paths: Vec<PathBuf> = state_for_shutdown.borrow().temp_attachment_files.iter().cloned().collect();
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
        // Message-drag files still in their grace period.
        sweep_drag_dir();
    });
    // And any a previous run left behind (a crash, or an exit mid-grace).
    gio::spawn_blocking(sweep_drag_dir);
    // A repaint hook for the folder pane, so a config change that alters it (a
    // saved search, item 2.4 / M-19) can rebuild the tree at once rather than
    // waiting for the next `FoldersUpdated`.
    state.borrow_mut().folder_tree_refresh = Some(Rc::new({
        let state = state.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        move || rebuild_folder_tree(&state, &folder_selection, &folder_scroller)
    }));
    let reading_stack = gtk::Stack::new();
    let state_clone = state.clone();
    let state_clone2 = state.clone();
    let reading_stack_clone = reading_stack.clone();
    // The profile card (Phase 4.2 / P-10, UX-67): a single context is built
    // once the calendar state exists further down and stored here, so every
    // person reference - including the message-list rows built before it (the
    // row factory only ever consults the provider at hover/click time, never
    // during setup).
    let profile_card_ctx: Rc<RefCell<Option<Rc<crate::profile_card::ProfileCardContext>>>> = Rc::new(RefCell::new(None));
    let profile_card_provider: Rc<dyn Fn() -> Option<Rc<crate::profile_card::ProfileCardContext>>> = {
        let holder = profile_card_ctx.clone();
        Rc::new(move || holder.borrow().clone())
    };
    let profile_card_provider_for_rows = profile_card_provider.clone();
    // The shared category master list (item 2.1), read from `settings.json`
    // at startup. Mutated by the Manage-categories editor and the Microsoft
    // 365 Graph sync, read by the Categorize menu, the row context menu and
    // every row's category dots.
    let tags = Rc::new(RefCell::new(crate::categories::CategorySet::from_defs(
        state.borrow().app_config.borrow().categories.clone(),
    )));
    // Publish the same set on `UiState` so the Microsoft 365 Graph sync and
    // the Config editor can reach it without threading it through every call.
    state.borrow_mut().categories = tags.clone();
    let message_list_for_rows = message_list.clone();
    let tags_for_bind = tags.clone();
    // What the message context menu (item 1.17) works with; every row's
    // right-click opens it.
    let message_menu_context = MessageMenuContext {
        state: state.clone(),
        worker: worker.clone(),
        reading_stack: reading_stack.clone(),
        message_list: message_list.clone(),
        tags: tags.clone(),
    };
    let message_menu_context_for_rows = message_menu_context.clone();
    // Declared here, ahead of the row factory below, so each row's `setup`
    // can bind its avatar/checkbox visibility to this button's `active`
    // state (see the property bindings inside `connect_setup`) - appended
    // into the message-list header, after the favorite star, further down
    // where the rest of that header's controls are built.
    let select_mode_button = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["checkbox-checked-symbolic", "list-add-symbolic", "edit-select-all-symbolic"]))
        .tooltip_text("Select messages")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // Select mode and the two display settings rows follow, shared with the
    // row factory so `bind` can apply them without a settings lookup per row
    // (see `RowDisplay`, `RowDisplayRefs`).
    let row_display: Rc<Cell<RowDisplay>> = Rc::new(Cell::new(RowDisplay::from_settings(&state.borrow().settings)));
    // Weak handles to every created row's display-dependent widgets, so a
    // change to Select mode, "Show sender images" or the preview line count
    // updates the rows already on screen in place (the list view recycles row
    // widgets and won't rebind them for an unrelated setting change).
    let row_display_registry: Rc<RefCell<Vec<RowDisplayRefs>>> = Rc::new(RefCell::new(Vec::new()));
    let row_display_registry_for_setup = row_display_registry.clone();
    // Decoded contact-photo textures, keyed by sender address. A miss (or a
    // contact with no photo) is cached too, so the address-book lookup and
    // the base64 decode run at most once per sender per session.
    let sender_photo_cache: Rc<RefCell<HashMap<String, Option<gtk::gdk::Texture>>>> = Rc::new(RefCell::new(HashMap::new()));
    let worker_for_rows = worker.clone();
    let message_factory = gtk::SignalListItemFactory::new();
    message_factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        // A row is either a date-section header or a message. Both branches
        // are built once here and toggled with `set_visible` rather than
        // being separate factories or a `Gtk.Stack` - a Stack would size
        // every row to the taller of the two, giving headers message height.
        let header_label = gtk::Label::builder().xalign(0.0).css_classes(["heading"]).build();
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&header_label));
        let header_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).margin_start(10).margin_end(10).build();
        header_box.add_css_class("message-section-header");
        header_box.append(&expander);

        // The unread accent bar: full row height, flush against the pane's
        // leading edge (which is why `.message-list > row` zeroes its
        // padding), colored only when the message is unread.
        let accent = gtk::Box::builder().width_request(3).vexpand(true).build();
        accent.add_css_class("message-accent-bar");
        // The avatar overlay: a coloured circle with the sender's initials,
        // and on top of it (when there is one) the contact's photo. A photo
        // with no paintable leaves the initials visible underneath, so the
        // same two children serve both cases. Both children carry their own
        // 32x32 square request (like the reading-pane header's avatar) so the
        // circle stays perfectly round whatever height the row allocates the
        // overlay in the three-line layout.
        let avatar_label = gtk::Label::builder()
            .css_classes(["avatar-circle"])
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let avatar_photo = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .css_classes(["avatar-photo"])
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_shrink(true)
            .build();
        let avatar = gtk::Overlay::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            // Top-aligned with the sender line so the avatar spans the row's
            // three (or four) lines, as Outlook's does.
            .valign(gtk::Align::Start)
            .margin_start(8)
            .margin_end(8)
            .child(&avatar_label)
            .build();
        avatar.add_css_class("message-avatar");
        avatar.add_overlay(&avatar_photo);
        // The avatar (initials and, when present, the contact's photo) is
        // derived from the sender's name/address, so privacy mode blurs it.
        crate::privacy::mark(&avatar);

        // The message row follows Outlook's three-line layout (UX-28): the
        // sender and the status icons on line one, the subject and received
        // time on line two, the body preview on line three, and - only when
        // the message is categorised - a chip strip beneath them.
        //
        // The sender expands rather than sitting in a fixed 180px column: the
        // subject no longer shares its line, so there is no column to align,
        // and a long sender should use the space the subject used to take.
        let sender_label = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        // Sender name, address and (in the unified view) account name all land
        // in this one label; privacy mode blurs the lot.
        crate::privacy::mark(&sender_label);
        // The attachment and meeting-invite indicators, drawn after the
        // sender on rows that carry either. Same footprint as the flag icon;
        // hidden (not just blank) when absent so a plain row takes no extra
        // width.
        let attachment_icon = gtk::Image::builder()
            .icon_name("mail-attachment-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Has attachments")
            .visible(false)
            .build();
        let calendar_icon = gtk::Image::builder()
            .icon_name("x-office-calendar-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Meeting invite")
            .visible(false)
            .build();
        // The `@` badge for a message that mentions the user (M-25). No
        // standard `at-symbolic` ships with Adwaita, so resolve the closest
        // themed glyph - the UX-80 icon decision can bundle a proper one later.
        let mention_icon = gtk::Image::builder()
            .icon_name(themed_icon_name(&["at-symbolic", "chat-message-new-symbolic", "user-info-symbolic"]))
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Mentions you")
            .visible(false)
            .build();
        let importance_icon = gtk::Image::builder()
            .icon_name("emblem-important-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("High importance")
            .visible(false)
            .build();
        let subject_label = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let date_label = gtk::Label::builder().xalign(1.0).valign(gtk::Align::Center).build();
        // Sits in the sender line's status cluster, so a pinned row shows its
        // marker without shifting the subject or date - hidden (not just
        // blank) on unpinned rows so it takes no width at all there.
        let pin_icon = gtk::Image::builder()
            .icon_name(pin_icon_name(true))
            .pixel_size(12)
            .css_classes(["message-pin-icon"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        // The follow-up flag's due label and icon (item 1.16), set in `bind`.
        let flag_label = gtk::Label::builder().css_classes(["caption"]).valign(gtk::Align::Center).visible(false).build();
        let flag_icon = gtk::Image::builder().pixel_size(12).valign(gtk::Align::Center).visible(false).build();

        // The category chip strip (UX-28): a line of its own under the
        // preview, one dot + name pill per category. Built once per row and
        // recoloured on bind; the whole line hides when the message has no
        // categories, so an uncategorised row is exactly three lines.
        let category_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .valign(gtk::Align::Center)
            .css_classes(["message-category-row"])
            .visible(false)
            .build();
        let category_chips: Vec<CategoryChip> = (0..MAX_CATEGORY_CHIPS)
            .map(|_| {
                let chip = CategoryChip::new();
                category_row.append(&chip.root);
                chip
            })
            .collect();
        let category_overflow = gtk::Label::builder().xalign(0.0).css_classes(["message-category-overflow"]).visible(false).build();
        category_row.append(&category_overflow);

        // Line 1: sender and the status-icon cluster.
        let sender_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["message-sender-line"])
            .build();
        sender_line.append(&sender_label);
        sender_line.append(&attachment_icon);
        sender_line.append(&calendar_icon);
        sender_line.append(&mention_icon);
        sender_line.append(&importance_icon);
        sender_line.append(&pin_icon);
        sender_line.append(&flag_label);
        sender_line.append(&flag_icon);

        // Line 2: subject, with the received time/date at the far right.
        let subject_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["message-subject-line"])
            .build();
        subject_line.append(&subject_label);
        subject_line.append(&date_label);

        // Line 3: the body preview, spanning the row and ellipsizing so the
        // snippet shows as much of the body as the pane's current width fits
        // and re-flows when the pane is resized - the cached text is
        // deliberately longer than any width can display (see
        // `PREVIEW_MAX_CHARS`).
        //
        // Always present, even when there's no preview text: an empty label
        // still reserves its line, so read and unread rows stay the same
        // height and the list doesn't ripple as previews arrive.
        let preview_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .css_classes(["message-preview"])
            .build();
        let preview_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["message-preview-line"])
            .build();
        preview_line.append(&preview_label);

        let text_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .margin_end(10)
            .css_classes(["message-text"])
            .build();
        text_column.append(&sender_line);
        text_column.append(&subject_line);
        text_column.append(&preview_line);
        text_column.append(&category_row);

        // Same footprint as the avatar it replaces (32x32, centered) so
        // toggling Select mode swaps one for the other in place rather than
        // adding a separate column.
        let checkbox = gtk::CheckButton::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .visible(false)
            .build();
        checkbox.add_css_class("message-select-checkbox");

        let message_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
        message_box.add_css_class("message-row");
        message_box.append(&accent);
        message_box.append(&avatar);
        message_box.append(&checkbox);
        message_box.append(&text_column);

        // The conversation-header row: its own expander (the section header's
        // expander lives in `header_box`) driving the same sender / subject /
        // preview lines a message row shows, with the participant count as the
        // extra column that marks a row as a thread rather than a message.
        let thread_sender = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        // Thread header sender (a name, or the participant list) - privacy
        // mode blurs it like the message rows' sender column.
        crate::privacy::mark(&thread_sender);
        let thread_attachment_icon = gtk::Image::builder()
            .icon_name("mail-attachment-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Has attachments")
            .visible(false)
            .build();
        let thread_calendar_icon = gtk::Image::builder()
            .icon_name("x-office-calendar-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Meeting invite")
            .visible(false)
            .build();
        let thread_mention_icon = gtk::Image::builder()
            .icon_name(themed_icon_name(&["at-symbolic", "chat-message-new-symbolic", "user-info-symbolic"]))
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Mentions you")
            .visible(false)
            .build();
        let thread_importance_icon = gtk::Image::builder()
            .icon_name("emblem-important-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("High importance")
            .visible(false)
            .build();
        let thread_subject = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let thread_count = gtk::Label::builder().xalign(0.0).css_classes(["message-thread-count"]).build();
        let thread_pin_icon = gtk::Image::builder()
            .icon_name(pin_icon_name(true))
            .pixel_size(12)
            .css_classes(["message-pin-icon"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let thread_flag_icon = gtk::Image::builder().pixel_size(12).valign(gtk::Align::Center).visible(false).build();
        let thread_date = gtk::Label::builder().xalign(1.0).valign(gtk::Align::Center).build();
        let thread_preview_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .css_classes(["message-preview"])
            .build();
        let thread_category_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .valign(gtk::Align::Center)
            .css_classes(["message-category-row"])
            .visible(false)
            .build();
        let thread_category_chips: Vec<CategoryChip> = (0..MAX_CATEGORY_CHIPS)
            .map(|_| {
                let chip = CategoryChip::new();
                thread_category_row.append(&chip.root);
                chip
            })
            .collect();
        let thread_category_overflow = gtk::Label::builder().xalign(0.0).css_classes(["message-category-overflow"]).visible(false).build();
        thread_category_row.append(&thread_category_overflow);

        // Line 1: participants and the aggregate status icons, then the
        // message count.
        let thread_sender_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["message-sender-line"])
            .build();
        thread_sender_line.append(&thread_sender);
        thread_sender_line.append(&thread_attachment_icon);
        thread_sender_line.append(&thread_calendar_icon);
        thread_sender_line.append(&thread_mention_icon);
        thread_sender_line.append(&thread_importance_icon);
        thread_sender_line.append(&thread_count);
        thread_sender_line.append(&thread_pin_icon);
        thread_sender_line.append(&thread_flag_icon);

        // Line 2: subject and the newest message's date.
        let thread_subject_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["message-subject-line"])
            .build();
        thread_subject_line.append(&thread_subject);
        thread_subject_line.append(&thread_date);

        // Line 3: the newest message's preview.
        let thread_preview_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["message-preview-line"])
            .build();
        thread_preview_line.append(&thread_preview_label);

        let thread_column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
        thread_column.append(&thread_sender_line);
        thread_column.append(&thread_subject_line);
        thread_column.append(&thread_preview_line);
        thread_column.append(&thread_category_row);

        let thread_expander = gtk::TreeExpander::new();
        thread_expander.set_child(Some(&thread_column));
        let thread_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).margin_start(14).margin_end(10).build();
        thread_box.add_css_class("message-thread-row");
        thread_box.append(&thread_expander);

        // The avatar/checkbox swap and the preview line count depend on
        // app-wide display settings (Select mode, "Show sender images",
        // preview lines), not on this row's message. Register weak handles so
        // a settings change can update every live row in place; `bind` also
        // applies them, so a freshly recycled row comes out correct. Both
        // preview labels are registered: the same row widget renders either a
        // message row or a conversation header.
        row_display_registry_for_setup.borrow_mut().push(RowDisplayRefs {
            avatar: avatar.downgrade(),
            checkbox: checkbox.downgrade(),
            preview_label: preview_label.downgrade(),
            thread_preview_label: thread_preview_label.downgrade(),
        });

        let row_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        row_box.append(&header_box);
        row_box.append(&message_box);
        row_box.append(&thread_box);

        // Action box (initially hidden). Anchored to the top-right so it sits
        // over the sender line's status icons - where Outlook's hover actions
        // appear - rather than over the subject line.
        let action_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_end(8)
            .margin_top(2)
            .build();
        action_box.add_css_class("hover-quick-actions");
        // Icons/tooltips below are placeholders overwritten by `connect_bind`
        // (`mark_read_btn`/`pin_btn`/`follow_up_btn`) before the row is ever
        // shown - each reflects this row's *current* message, not a fixed
        // action, so there's nothing meaningful to set until bind time.
        let mark_read_btn = gtk::Button::from_icon_name("mail-mark-read-symbolic");
        mark_read_btn.add_css_class("hover-quick-action");
        let follow_up_btn = gtk::Button::from_icon_name(follow_up_icon_name(false));
        follow_up_btn.add_css_class("hover-quick-action");
        follow_up_btn.set_tooltip_text(Some("Flag"));
        let pin_btn = gtk::Button::from_icon_name(pin_icon_name(false));
        pin_btn.add_css_class("hover-quick-action");
        pin_btn.set_tooltip_text(Some("Pin/Unpin"));
        let delete_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        delete_btn.add_css_class("hover-quick-action");
        delete_btn.set_tooltip_text(Some("Delete"));
        let reply_btn = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
        reply_btn.add_css_class("hover-quick-action");
        reply_btn.set_tooltip_text(Some("Reply"));
        action_box.append(&mark_read_btn);
        action_box.append(&follow_up_btn);
        action_box.append(&pin_btn);
        action_box.append(&delete_btn);
        action_box.append(&reply_btn);
        // Initially hide actions
        action_box.set_visible(false);
        // Overlay to overlay actions on top of content
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&row_box));
        overlay.add_overlay(&action_box);

        // Which message this row is currently showing. Written by `bind`,
        // read by the handlers below *at click time* - which is what lets
        // every signal be connected once here instead of on every rebind.
        // The old per-bind connections accumulated on recycled rows (nothing
        // ever disconnected them), so a scrolled list eventually fired one
        // click at several messages at once.
        let bound: Rc<RefCell<Option<Rc<EmailSummary>>>> = Rc::new(RefCell::new(None));

        // The conversation this row heads, when it's a conversation header.
        let bound_thread: Rc<RefCell<Option<ThreadId>>> = Rc::new(RefCell::new(None));

        // The profile card (Phase 4.2 / UX-67): hovering or clicking a row's
        // sender name/avatar opens the person's card. Installed once per row
        // (not per bind), resolving the row's message lazily from `bound`.
        {
            let provider = profile_card_provider_for_rows.clone();
            let resolve: crate::profile_card::PersonResolver = {
                let bound = bound.clone();
                Rc::new(move || {
                    bound
                        .borrow()
                        .as_ref()
                        .and_then(|summary| summary.from.first().map(|sender| (sender.clone(), mailbox_account_id(&summary.mailbox))))
                })
            };
            crate::profile_card::attach_person_trigger(provider.clone(), &sender_label, false, resolve.clone());
            crate::profile_card::attach_person_trigger(provider, &avatar, false, resolve);
        }

        let checkbox_suppress: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            // Model -> checkbox, live: ctrl/shift-clicking a row already on
            // screen changes `list_item`'s `selected` property, which
            // `bind()` alone wouldn't see again until this row is recycled.
            let checkbox = checkbox.clone();
            let suppress = checkbox_suppress.clone();
            list_item.connect_selected_notify(move |li| {
                suppress.set(true);
                checkbox.set_active(li.is_selected());
                suppress.set(false);
            });
        }
        {
            // Checkbox -> model: an alternate input for the same selection
            // the row's own click/ctrl-click/shift-click already drives,
            // never a second, parallel selection concept. Weak, not a
            // strong clone: `list_item` already owns `checkbox` transitively
            // through the widget tree it holds as its child, so a strong
            // capture here would close a reference cycle
            // (list_item -> ... -> checkbox -> this closure -> list_item)
            // that would never be freed.
            let selection = message_list_for_rows.selection.clone();
            let list_item_weak = list_item.downgrade();
            let suppress = checkbox_suppress.clone();
            checkbox.connect_toggled(move |cb| {
                if suppress.get() {
                    return;
                }
                let Some(list_item) = list_item_weak.upgrade() else { return };
                let pos = list_item.position();
                if pos == gtk::INVALID_LIST_POSITION {
                    return;
                }
                if cb.is_active() {
                    selection.select_item(pos, false);
                } else {
                    selection.unselect_item(pos);
                }
            });
        }

        {
            // The message context menu (item 1.17) on right-click.
            let bound = bound.clone();
            let bound_thread = bound_thread.clone();
            let context = message_menu_context_for_rows.clone();
            let list_item_weak = list_item.downgrade();
            let context_menu = gtk::GestureClick::new();
            context_menu.set_button(gtk::gdk::BUTTON_SECONDARY);
            context_menu.connect_pressed(move |gesture, _, x, y| {
                let Some(anchor) = gesture.widget() else { return };
                let position = list_item_weak
                    .upgrade()
                    .map(|list_item| list_item.position())
                    .filter(|pos| *pos != gtk::INVALID_LIST_POSITION);
                let target = match (bound.borrow().as_deref().cloned(), bound_thread.borrow().clone()) {
                    (Some(summary), _) => MessageMenuTarget::Row {
                        summary: Box::new(summary),
                        position,
                    },
                    (None, Some(thread)) => MessageMenuTarget::Thread(thread),
                    (None, None) => return,
                };
                if open_message_menu(&context, &anchor, Some((x, y)), target) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            });
            overlay.add_controller(context_menu);
        }

        // Drag source for every message row: dragging a message - or, when
        // the dragged row is part of a multi-selection, the whole selection -
        // offers two payloads in one provider union: the internal
        // `(mailbox, uid)` list for folder drops (see the folder rows' drop
        // target), and the messages as `.eml` files so the drag can land in
        // a file manager (or a composer) too. GTK's union provider lets each
        // drop target pick whichever format it understands. The files are
        // written lazily, off the GTK thread, only when a target asks for
        // them (see `MessageFilesProvider`); messages whose raw bytes aren't
        // cached yet are left out.
        {
            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE);
            let bound_for_drag = bound.clone();
            let state_for_drag = state_clone.clone();
            let message_list_for_drag = message_list_for_rows.clone();
            let current_drag_files: Rc<RefCell<Option<Rc<DragFiles>>>> = Rc::new(RefCell::new(None));
            let current_drag_files_for_prepare = current_drag_files.clone();
            drag_source.connect_prepare(move |_source, _x, _y| {
                let summaries = dragged_summaries(&message_list_for_drag, &bound_for_drag);
                if summaries.is_empty() {
                    return None;
                }
                let internal: Vec<(String, u32)> = summaries.iter().map(|s| (s.mailbox.0.clone(), s.uid.0)).collect();
                let bytes = match serde_json::to_vec(&internal) {
                    Ok(bytes) => glib::Bytes::from(bytes.as_slice()),
                    Err(e) => {
                        tracing::warn!("failed to serialize message drag payload: {e}");
                        return None;
                    }
                };
                // The provider holds the payload as a `G_TYPE_BYTES` *value*
                // (rather than exposing it under a private mime type), because
                // GTK's drop targets only accept a drag when a GType can be
                // matched between the two sides' formats - a mime-only
                // `ContentFormats` has no gtypes, and an unregistered mime has
                // no GType mapping, so the folder rows would reject every
                // drop before `connect_drop` ever fired. The value is copied
                // straight into the drop handler on a local drop, no stream
                // round-trip needed.
                let internal_provider = gtk::gdk::ContentProvider::for_value(&glib::Value::from(bytes));
                let messages = drag_messages(&state_for_drag.borrow(), &summaries);
                if messages.is_empty() {
                    return Some(internal_provider);
                }
                let files = DragFiles::new(messages);
                let previous = current_drag_files_for_prepare.borrow_mut().replace(files.clone());
                if let Some(previous) = previous {
                    previous.finish();
                }
                Some(gtk::gdk::ContentProvider::new_union(&[internal_provider, MessageFilesProvider::new(files).upcast()]))
            });
            // The `.eml` files written for the drag-out are cleaned up once
            // the drag finishes, whatever its outcome, after a grace period
            // for the file manager's copy.
            {
                let current_drag_files = current_drag_files.clone();
                drag_source.connect_drag_end(move |_source, _drag, _delete| {
                    let files = current_drag_files.borrow_mut().take();
                    if let Some(files) = files {
                        files.finish();
                    }
                });
            }
            overlay.add_controller(drag_source);
        }

        {
            let action_box = action_box.clone();
            let message_box = message_box.clone();
            let hover_controller = gtk::EventControllerMotion::new();
            let action_box_for_leave = action_box.clone();
            hover_controller.connect_enter(move |_, _, _| {
                // Section headers have no quick actions.
                if message_box.is_visible() {
                    action_box.set_visible(true);
                }
            });
            hover_controller.connect_leave(move |_| {
                action_box_for_leave.set_visible(false);
            });
            overlay.add_controller(hover_controller);
        }

        {
            let state = state_clone.clone();
            let bound = bound.clone();
            let message_list = message_list_for_rows.clone();
            mark_read_btn.connect_clicked(move |btn| {
                let Some((mailbox, uid, mark_read)) = bound.borrow().as_ref().map(|s| (s.mailbox.clone(), s.uid, s.is_unread())) else {
                    return;
                };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
                let Some(cmd_tx) = cmd_tx else { return };
                optimistic_toggle_read(&state, &message_list, &mailbox, &[uid], mark_read);
                let (add, remove) = if mark_read {
                    (vec![SystemFlagBit::Seen], Vec::new())
                } else {
                    (Vec::new(), vec![SystemFlagBit::Seen])
                };
                // Undo: the opposite `\Seen` change.
                record_undo(
                    &state,
                    if mark_read { "Marked as read" } else { "Marked as unread" },
                    vec![AccountCommand::StoreFlagsMany {
                        mailbox: mailbox.clone(),
                        uids: vec![uid],
                        add: remove.clone(),
                        remove: add.clone(),
                    }],
                );
                let _ = cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
                    mailbox,
                    uids: vec![uid],
                    add,
                    remove,
                });
                // `optimistic_toggle_read`'s repopulate will also rebind this
                // row (refreshing the button via `connect_bind` like the
                // sender/subject bold state already does), but flip the icon
                // now too so there's no visible lag.
                let (icon, tooltip) = mark_read_button_icon(!mark_read);
                btn.set_icon_name(icon);
                btn.set_tooltip_text(Some(tooltip));
            });
        }
        {
            // Flag / Clear flag on this row's message (item 1.16). The
            // optimistic repopulate rebinds the row, which repaints the
            // button from the new state.
            let state = state_clone.clone();
            let bound = bound.clone();
            let message_list = message_list_for_rows.clone();
            follow_up_btn.connect_clicked(move |_| {
                let Some(summary) = bound.borrow().as_deref().cloned() else { return };
                toggle_follow_up(&state, &message_list, vec![summary]);
            });
        }
        {
            let state = state_clone.clone();
            let bound = bound.clone();
            let message_list = message_list_for_rows.clone();
            pin_btn.connect_clicked(move |_btn| {
                let Some((mailbox, uid, pinning)) = bound.borrow().as_ref().map(|s| (s.mailbox.clone(), s.uid, !s.is_pinned())) else {
                    return;
                };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                // Pin isn't available on the Graph mail transport (5.7.7).
                if state.borrow().accounts.get(&account_id).is_some_and(|handle| handle.uses_graph_mail) {
                    return;
                }
                let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
                let Some(cmd_tx) = cmd_tx else { return };
                // Patches the model (and, via the Pinned group, this
                // message's whole position in the list) immediately rather
                // than waiting on a server-confirmed resync - unlike a plain
                // icon flip, this can move the row to a different section
                // entirely, so there's no safe "flip this widget's icon too"
                // shortcut left to take here: whatever row GTK recycles this
                // very button onto during that repopulate gets its icon from
                // `connect_bind`, not from this handler.
                optimistic_toggle_pinned(&state, &message_list, &mailbox, &[uid], pinning);
                let (add, remove) = if pinning {
                    (vec![SystemFlagBit::Flagged], Vec::new())
                } else {
                    (Vec::new(), vec![SystemFlagBit::Flagged])
                };
                // Undo: the opposite `\Flagged` change.
                record_undo(
                    &state,
                    if pinning { "Pinned" } else { "Unpinned" },
                    vec![AccountCommand::StoreFlagsMany {
                        mailbox: mailbox.clone(),
                        uids: vec![uid],
                        add: remove.clone(),
                        remove: add.clone(),
                    }],
                );
                let _ = cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
                    mailbox,
                    uids: vec![uid],
                    add,
                    remove,
                });
            });
        }
        {
            let state = state_clone.clone();
            let bound = bound.clone();
            let message_list = message_list_for_rows.clone();
            delete_btn.connect_clicked(move |_| {
                let Some((mailbox, uid)) = bound.borrow().as_ref().map(|s| (s.mailbox.clone(), s.uid)) else {
                    return;
                };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
                if let Some(cmd_tx) = cmd_tx {
                    optimistic_remove_messages(&state, &message_list, &mailbox, &[uid]);
                    // Undo: move the message back out of Trash.
                    if let Some(trash) = role_mailbox_id(&state, &account_id, MailboxRole::Trash) {
                        record_undo(
                            &state,
                            &undo_move_description(MailboxRole::Trash, 1),
                            vec![AccountCommand::RevertMove {
                                moved_from: mailbox.clone(),
                                moved_to: trash,
                                uids: vec![uid],
                            }],
                        );
                    }
                    let _ = cmd_tx.send_blocking(AccountCommand::MoveMessage {
                        mailbox,
                        uid,
                        role: MailboxRole::Trash,
                    });
                }
            });
        }
        {
            let state = state_clone.clone();
            let worker = worker_for_rows.clone();
            let reading_stack = reading_stack_clone.clone();
            let bound = bound.clone();
            reply_btn.connect_clicked(move |_| {
                let Some(summary) = bound.borrow().clone() else { return };
                // Everything the composer needs is read out first and the
                // borrow dropped: `show_composer_in_reading_pane` takes its
                // own `borrow_mut` to install the draft-confirmation relay.
                let opened = {
                    let mut st = state.borrow_mut();
                    // Only possible once the body has arrived - the composer
                    // needs it to quote. Silently a no-op until then.
                    let body = st.body_cache.get(&summary.mailbox, &summary.uid);
                    body.and_then(|body| {
                        let account_id = mailbox_account_id(&summary.mailbox)?;
                        let handle = st.accounts.get(&account_id)?;
                        let from_email = handle.email.clone();
                        let cmd_tx = handle.cmd_tx.clone();
                        let prefill = crate::compose::build_reply_prefill(&summary, &body, &from_email, crate::compose::ReplyMode::Reply);
                        Some((from_email, cmd_tx, prefill, st.rich_text_default))
                    })
                };
                // Routed through the shared opener rather than building the
                // composer inline: that's what replaces any composer already
                // in the pane (so its autosave loop stops), restores the
                // previous page on close, and owns the `draft_saved_tx` slot.
                if let Some((_from_email, cmd_tx, prefill, rich_text_default)) = opened {
                    show_composer_in_reading_pane(
                        &state,
                        &worker,
                        &reading_stack,
                        "Reply",
                        cmd_tx,
                        prefill,
                        rich_text_default,
                        mailbox_account_id(&summary.mailbox),
                        Some(crate::config_view::SignatureDefault::Reply),
                    );
                }
            });
        }

        unsafe {
            list_item.set_data(
                "row-widgets",
                MessageRowWidgets {
                    header_box,
                    expander,
                    header_label,
                    message_box,
                    thread_box,
                    thread_expander,
                    thread_sender,
                    thread_attachment_icon,
                    thread_calendar_icon,
                    thread_mention_icon,
                    thread_importance_icon,
                    thread_subject,
                    thread_count,
                    thread_pin_icon,
                    thread_flag_icon,
                    thread_date,
                    thread_preview_label,
                    thread_category_row,
                    thread_category_chips,
                    thread_category_overflow,
                    checkbox,
                    checkbox_suppress,
                    accent,
                    avatar,
                    avatar_label,
                    avatar_photo,
                    sender_label,
                    attachment_icon,
                    calendar_icon,
                    mention_icon,
                    importance_icon,
                    subject_label,
                    pin_icon,
                    flag_label,
                    flag_icon,
                    category_row,
                    category_chips,
                    category_overflow,
                    date_label,
                    preview_label,
                    action_box,
                    mark_read_btn,
                    follow_up_btn,
                    pin_btn,
                    bound_thread,
                    bound,
                },
            );
        }
        list_item.set_child(Some(&overlay));
    });
    let row_display_for_bind = row_display.clone();
    let sender_photo_cache_for_bind = sender_photo_cache.clone();
    message_factory.connect_bind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else { return };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let widgets = unsafe { list_item.data::<MessageRowWidgets>("row-widgets").expect("row widgets").as_ref().clone() };
        let item = boxed.borrow::<MessageItem>();
        match &*item {
            MessageItem::Section(section) => {
                widgets.header_box.set_visible(true);
                widgets.message_box.set_visible(false);
                widgets.thread_box.set_visible(false);
                widgets.action_box.set_visible(false);
                widgets.category_row.set_visible(false);
                widgets.thread_category_row.set_visible(false);
                // What actually draws the disclosure chevron and drives
                // expand/collapse. The expansion *state* is applied in
                // `MessageListModel::repopulate`, not here - bind only runs
                // for rows in the viewport, so doing it here would leave
                // scrolled-off sections unexpanded and the list's row count
                // wrong.
                widgets.expander.set_list_row(Some(&row));
                widgets.header_label.set_label(&section.label);
                // Headers aren't a selection target for the mouse:
                // `set_selectable(false)` is a `ListItem` property enforced
                // by `GtkListView`'s own click/ctrl-click/shift-click
                // handling, independent of which `SelectionModel` backs the
                // view, so a header can never enter `MultiSelection`'s
                // selection set via the mouse. `SelectionKind::Section`
                // stays a defensive no-op case in the selection handler
                // regardless, in case a header is ever selected some other
                // way.
                list_item.set_selectable(false);
                list_item.set_activatable(false);
                *widgets.bound.borrow_mut() = None;
                *widgets.bound_thread.borrow_mut() = None;
            }
            MessageItem::Message(summary) => {
                widgets.header_box.set_visible(false);
                widgets.message_box.set_visible(true);
                widgets.thread_box.set_visible(false);
                // Must be set explicitly, not just left alone: these are
                // `ListItem` properties that survive widget recycling, so a
                // row that last rendered a header would stay unclickable.
                list_item.set_selectable(true);
                list_item.set_activatable(true);
                // Authoritative reset on every bind, not just a one-time
                // connection: recycling a row must not leave it showing the
                // previous occupant's checked state, and this is cheaper
                // than reasoning about signal-timing races across recycles.
                widgets.checkbox_suppress.set(true);
                widgets.checkbox.set_active(list_item.is_selected());
                widgets.checkbox_suppress.set(false);

                let sender = summary.from.first();
                match sender {
                    Some(address) => {
                        widgets.avatar_label.set_label(&crate::message_header::initials(address.name.as_deref(), &address.address));
                        for class in crate::message_header::AVATAR_COLOR_CLASSES {
                            widgets.avatar_label.remove_css_class(class);
                        }
                        widgets.avatar_label.add_css_class(crate::message_header::avatar_color_class(&address.address));
                        match sender_photo_texture(&state_clone2, &address.address, &sender_photo_cache_for_bind) {
                            Some(texture) => widgets.avatar_photo.set_paintable(Some(&texture)),
                            None => widgets.avatar_photo.set_paintable(None::<&gtk::gdk::Texture>),
                        }
                    }
                    None => {
                        widgets.avatar_label.set_label("?");
                        widgets.avatar_photo.set_paintable(None::<&gtk::gdk::Texture>);
                    }
                }

                let from = sender.map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
                // In the unified "All Inboxes" view, stamp each row with its
                // owning account so mail from mixed mailboxes stays readable.
                let account_label = {
                    let st = state_clone2.borrow();
                    if matches!(st.mail_view, MailView::UnifiedInbox) {
                        mailbox_account_id(&summary.mailbox).and_then(|id| st.accounts.get(&id)).map(|h| {
                            if h.display_name.is_empty() {
                                h.email.clone()
                            } else {
                                h.display_name.clone()
                            }
                        })
                    } else {
                        None
                    }
                };
                widgets.sender_label.set_label(&match account_label {
                    Some(acc) => format!("{from} · {acc}"),
                    None => from,
                });
                widgets.subject_label.set_label(summary.subject.as_deref().unwrap_or("(no subject)"));
                // The attachment and invite indicators: explicit on every
                // bind, so a recycled row never shows the previous
                // occupant's icons. Both are independent - a named `invite.ics`
                // attachment is both.
                widgets.attachment_icon.set_visible(summary.has_attachment);
                widgets.calendar_icon.set_visible(summary.has_calendar);
                widgets.mention_icon.set_visible(summary.mentions);
                let (imp_icon, imp_tip) = importance_icon_and_tooltip(summary.importance);
                widgets.importance_icon.set_icon_name(Some(imp_icon));
                widgets.importance_icon.set_tooltip_text(Some(imp_tip));
                widgets.importance_icon.set_visible(summary.importance != lookout_core::Importance::Normal);
                widgets.date_label.set_label(&format_row_date(summary.date, chrono::Utc::now()));
                widgets.preview_label.set_label(summary.preview.as_deref().unwrap_or(""));
                // Display settings that don't change per message: the preview
                // line count, and whether the avatar is shown at all.
                row_display_for_bind.get().apply(&widgets.avatar, &widgets.checkbox, &widgets.preview_label);

                let unread = summary.is_unread();
                widgets.sender_label.set_css_classes(sender_label_classes(unread));
                widgets
                    .subject_label
                    .set_css_classes(if unread { &["message-subject-unread"] } else { &["message-subject-read"] });
                widgets.date_label.set_css_classes(if unread {
                    &["caption", "message-date-unread"]
                } else {
                    &["caption", "message-date-read"]
                });
                if unread {
                    widgets.accent.add_css_class("unread");
                } else {
                    widgets.accent.remove_css_class("unread");
                }
                widgets.pin_icon.set_visible(summary.is_pinned());
                set_pin_button_state(&widgets.pin_btn, summary.is_pinned());
                // Pin has no Graph counterpart (item 5.7.7): disable the
                // hover action rather than letting it appear to work.
                let graph_mail = state_slot.borrow().as_ref().is_some_and(|state| mailbox_uses_graph_mail(&state.borrow(), &summary.mailbox));
                widgets.pin_btn.set_sensitive(!graph_mail);
                widgets
                    .pin_btn
                    .set_tooltip_text(Some(if graph_mail { "Pin isn't available on Graph mail" } else { "Pin/Unpin" }));
                let (mr_icon, mr_tooltip) = mark_read_button_icon(unread);
                widgets.mark_read_btn.set_icon_name(mr_icon);
                widgets.mark_read_btn.set_tooltip_text(Some(mr_tooltip));
                let follow_up = summary.follow_up();
                apply_flag_indicator(&widgets.flag_icon, Some(&widgets.flag_label), follow_up.as_ref());
                set_flag_button_state(&widgets.follow_up_btn, follow_up.as_ref());

                // Show the row's category chips: one dot + name pill per
                // category, with the rest folded into "+N". The strip hides
                // itself when the message has none (UX-28).
                let tags_borrow = tags_for_bind.borrow();
                let shown = crate::categories::categories_for_keywords(&tags_borrow, &summary.keywords);
                bind_category_chips(&widgets.category_row, &widgets.category_chips, &widgets.category_overflow, &shown);

                *widgets.bound.borrow_mut() = Some(Rc::clone(summary));
                *widgets.bound_thread.borrow_mut() = None;
            }
            MessageItem::Thread(thread) => {
                widgets.header_box.set_visible(false);
                widgets.message_box.set_visible(false);
                widgets.thread_box.set_visible(true);
                widgets.action_box.set_visible(false);
                // A conversation header is a selection target (item 1.7):
                // selecting it shows the whole conversation in the reading
                // pane, and verbs act on every message in it, as in
                // Outlook; its `TreeExpander` arrow expands/collapses it.
                // The batch actions' checkboxes and the hover quick actions
                // key off the row being a message, so a thread row shows
                // neither.
                list_item.set_selectable(true);
                list_item.set_activatable(true);
                *widgets.bound.borrow_mut() = None;
                *widgets.bound_thread.borrow_mut() = Some(thread.id.clone());
                widgets.thread_expander.set_list_row(Some(&row));
                widgets.thread_sender.set_label(&thread.sender);
                widgets.thread_attachment_icon.set_visible(thread.has_attachment);
                widgets.thread_calendar_icon.set_visible(thread.has_calendar);
                widgets.thread_mention_icon.set_visible(thread.has_mentions);
                let (imp_icon, imp_tip) = importance_icon_and_tooltip(thread.importance);
                widgets.thread_importance_icon.set_icon_name(Some(imp_icon));
                widgets.thread_importance_icon.set_tooltip_text(Some(imp_tip));
                widgets.thread_importance_icon.set_visible(thread.importance != lookout_core::Importance::Normal);
                widgets.thread_subject.set_label(thread.subject.as_deref().unwrap_or("(no subject)"));
                widgets.thread_count.set_label(&thread.count.to_string());
                // Who's in the conversation, a hover away - on the row itself
                // rather than the count badge, so the whole header answers it.
                widgets
                    .thread_box
                    .set_tooltip_text(Some(&format!("{} messages: {}", thread.count, thread.participants.join(", "))));
                widgets.thread_date.set_label(&format_row_date(thread.latest, chrono::Utc::now()));
                widgets.thread_preview_label.set_label(thread.preview.as_deref().unwrap_or(""));
                // The header's preview follows the same "Message preview"
                // line-count setting as a message row's.
                row_display_for_bind.get().apply(&widgets.avatar, &widgets.checkbox, &widgets.thread_preview_label);
                // The newest message's category chips, on their own line under
                // the preview, exactly like a message row's strip.
                let tags_borrow = tags_for_bind.borrow();
                let shown = crate::categories::categories_for_keywords(&tags_borrow, &thread.tags);
                bind_category_chips(&widgets.thread_category_row, &widgets.thread_category_chips, &widgets.thread_category_overflow, &shown);
                widgets.thread_pin_icon.set_visible(thread.has_pinned);
                apply_flag_indicator(&widgets.thread_flag_icon, None, thread.follow_up.as_ref());
                // Unread bolds the whole header like it bolds a message row:
                // an unread member makes the conversation's sender, subject,
                // and date read as unread.
                let unread = thread.has_unread;
                widgets.thread_sender.set_css_classes(sender_label_classes(unread));
                widgets
                    .thread_subject
                    .set_css_classes(if unread { &["message-subject-unread"] } else { &["message-subject-read"] });
                widgets.thread_date.set_css_classes(if unread {
                    &["caption", "message-date-unread"]
                } else {
                    &["caption", "message-date-read"]
                });
            }
        }
    });
    message_factory.connect_unbind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(widgets) = (unsafe { list_item.data::<MessageRowWidgets>("row-widgets") }) else {
            return;
        };
        let widgets = unsafe { widgets.as_ref() };
        *widgets.bound.borrow_mut() = None;
        *widgets.bound_thread.borrow_mut() = None;
        widgets.action_box.set_visible(false);
        // Don't keep a recycled row pinned to a `TreeListRow` it no longer
        // renders.
        widgets.expander.set_list_row(None);
        widgets.thread_expander.set_list_row(None);
    });
    let message_list_view = gtk::ListView::new(Some(message_list.selection.clone()), Some(message_factory));
    message_list_view.add_css_class("message-list");
    message_list_view.add_css_class(crate::settings::spacing_class(&settings.get_string(crate::settings::LAYOUT_SPACING)));
    // Never scrolls sideways: every row's text ellipsizes to the pane's
    // width instead. Without pinning this, the preview line's natural width
    // request - it holds far more text than fits, by design, so the snippet
    // runs to the pane's edge at any width - would let the list report a
    // huge natural width and grow a horizontal scrollbar.
    let message_scroller = gtk::ScrolledWindow::builder()
        .child(&message_list_view)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    // The model's one window onto its widgets: it needs the adjustment to tell
    // whether the user is at the top of the list when a rebuild lands, and the
    // view to put them back there if they were (see `pin_to_top`).
    message_list.attach_scroller(&message_scroller, &message_list_view);
    // Shift+F10 / the Menu key open the message context menu (item 1.17) on
    // the selection, pointing at the focused row - the keyboard's
    // right-click (UX-90).
    {
        let context = message_menu_context.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |controller, key, _, modifiers| {
            let menu_key = key == gtk::gdk::Key::Menu || (key == gtk::gdk::Key::F10 && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK));
            if !menu_key {
                return glib::Propagation::Proceed;
            }
            let Some(list_view) = controller.widget() else { return glib::Propagation::Proceed };
            let point = list_view
                .root()
                .and_then(|root| root.focus())
                .filter(|focus| focus.is_ancestor(&list_view))
                .and_then(|focus| focus.compute_bounds(&list_view))
                .map(|bounds| (f64::from(bounds.x() + 24.0), f64::from(bounds.y() + bounds.height() / 2.0)));
            if open_message_menu(&context, &list_view, point, MessageMenuTarget::Selection) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        message_list_view.add_controller(keys);
    }
    // Double-click / Enter on a message opens it in its own window (item
    // 1.9, Outlook's UX-37): `GtkListView::activate` fires for both, and the
    // row factory's `bind` has already made message rows activatable and
    // header rows not, so a header can't get here. The window itself is
    // built on demand; the main window (the replies' composer destination)
    // is resolved from the view's root at activation time, since it doesn't
    // exist yet in `build_window`'s scope here.
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        message_list_view.connect_activate(move |view, position| {
            let Some(summary) = message_list.summary_at_position(position) else { return };
            // A draft opens in the composer, not the read-only window (item
            // 1.12): editing is the only useful thing to do with one.
            if summary_is_draft(&state.borrow(), &summary) {
                open_draft_composer(&state, &reading_stack, &summary);
                return;
            }
            let Some(parent) = view.root().and_downcast::<adw::ApplicationWindow>() else { return };
            crate::message_window::open_message_window(&parent, &state, &worker, &reading_stack, &summary);
        });
    }
    // Shown instead of the list while the folder on screen is empty and its
    // sync is still outstanding - see `refresh_message_loading_state`.
    // Without this, the empty list a folder switch eagerly paints (see
    // `select_mailbox`) looks identical to a folder that's genuinely empty
    // for however long the live IMAP fetch takes.
    let message_loading_spinner = gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build();
    let message_loading_label = gtk::Label::builder().label("Fetching message list.").css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&message_loading_label);
    let message_loading_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build();
    message_loading_box.append(&message_loading_spinner);
    message_loading_box.append(&message_loading_label);
    // Shown instead of the list when the folder on screen is genuinely empty
    // (or nothing is selected at all) - see `refresh_message_loading_state`,
    // which picks between "loading" and "empty" by whether a sync is still
    // outstanding.
    let message_empty_icon = svg_image(
        "/io/github/gavindi/Lookout/icons/empty-bird-1.svg",
        include_bytes!("../../../data/resources/icons/empty-bird-1.svg"),
        96,
    );
    message_empty_icon.add_css_class("empty-state-icon");
    let message_empty_title = gtk::Label::builder().label("Folder is empty").css_classes(["heading"]).build();
    let message_empty_subtitle = gtk::Label::builder().label("The stars were bright, Fernando.").css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&message_empty_title);
    crate::privacy::mark_ui(&message_empty_subtitle);
    let message_empty_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build();
    message_empty_box.append(&message_empty_icon);
    message_empty_box.append(&message_empty_title);
    message_empty_box.append(&message_empty_subtitle);
    let message_list_stack = gtk::Stack::new();
    message_list_stack.add_named(&message_scroller, Some("list"));
    message_list_stack.add_named(&message_loading_box, Some("loading"));
    message_list_stack.add_named(&message_empty_box, Some("empty"));
    message_list_stack.set_visible_child_name("list");
    // Header row atop the message list: what's being shown on the left (the
    // folder's name over its account, plus the favorite star), and the list's
    // own controls on the right - sync, filter, sort direction, sort key.
    // `title_column`'s hexpand is what pushes the control cluster to the
    // trailing edge, so no spacer widget is needed.
    let folder_title_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .label("Inbox")
        .build();
    let account_title_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    // The list header's subtitle is the account's display name or address.
    crate::privacy::mark(&account_title_label);
    let title_column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    title_column.append(&folder_title_label);
    title_column.append(&account_title_label);

    let favorite_button = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"]))
        .tooltip_text("Add to Favorites")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // `refresh_list_header` sets the star's state programmatically on every
    // view change; without this guard that would fire `toggled` and re-write
    // the favorites set from under the user.
    let favorite_suppress = Rc::new(Cell::new(false));

    let sync_button = gtk::Button::builder()
        .icon_name(themed_icon_name(&["view-refresh-symbolic", "emblem-synchronizing-symbolic"]))
        .tooltip_text("Sync")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // The filter menu: one checkable item per Filter, each bound to its own
    // stateful boolean action (win.filter-unread, …). Filters combine, so the
    // menu is a set of independent checks, not a radio group - matching
    // Outlook's Filter menu. `MessageListModel` owns the active set - it's
    // applied inside `repopulate`, the single choke point every rebuild passes
    // through, against the model's unfiltered source of truth.
    let initial_filters = FilterSet::from_states(&state.borrow().settings.get_strv(crate::settings::MAIL_FILTERS));
    message_list.set_filter(initial_filters);
    let list_filter_menu = gio::Menu::new();
    for filter in Filter::ALL {
        let action = format!("win.filter-{}", filter.action_state());
        list_filter_menu.append(Some(filter.label()), Some(action.as_str()));
    }
    let list_filter_button = gtk::MenuButton::builder()
        .label(filter_button_label(initial_filters))
        .tooltip_text("Filter")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&list_filter_menu)
        .build();
    let sort_direction_button = gtk::ToggleButton::builder()
        .icon_name(sort_direction_icon(true))
        .tooltip_text(sort_direction_tooltip(state.borrow().sort_key, true))
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .active(true)
        .build();
    let sort_key_menu = gio::Menu::new();
    sort_key_menu.append(Some("By Date"), Some("win.sort-key('date')"));
    sort_key_menu.append(Some("By Sender"), Some("win.sort-key('sender')"));
    sort_key_menu.append(Some("By Subject"), Some("win.sort-key('subject')"));
    sort_key_menu.append(Some("By Size"), Some("win.sort-key('size')"));
    sort_key_menu.append(Some("By Importance"), Some("win.sort-key('importance')"));
    sort_key_menu.append(Some("By Flag"), Some("win.sort-key('flag')"));
    sort_key_menu.append(Some("By Category"), Some("win.sort-key('category')"));
    sort_key_menu.append(Some("By To"), Some("win.sort-key('to')"));
    sort_key_menu.append(Some("By Attachments"), Some("win.sort-key('attachments')"));
    let sort_key_button = gtk::MenuButton::builder()
        .label(state.borrow().sort_key.label())
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&sort_key_menu)
        .build();
    // The button's `active` state drives each row's avatar/checkbox swap; the
    // shared `row_display` and the row registry make the swap reach rows
    // already on screen (see `update_row_display`). The one side effect that
    // isn't per-row is clearing the selection on exit.
    {
        let message_list = message_list.clone();
        let row_display = row_display.clone();
        let row_display_registry = row_display_registry.clone();
        select_mode_button.connect_toggled(move |button| {
            let display = RowDisplay {
                select_mode: button.is_active(),
                ..row_display.get()
            };
            row_display.set(display);
            update_row_display(&row_display_registry, display);
            if !button.is_active() {
                // Leaving Select mode clears the selection - matches Gmail's
                // "cancel selection" behavior and avoids stranding the
                // reading pane on the "N selected" placeholder after the
                // toggle that revealed it (and every row's checkbox) is gone.
                message_list.selection.unselect_all();
            }
        });
    }

    // Focused / Other tabs (M-03 / item 5.4): shown only for an Inbox of an
    // account that provides Focused Inbox (see `refresh_list_header`). Two
    // linked radio-style toggles; Focused is selected when they appear, which
    // is safe even when Outlook's Focused Inbox is off (every message then
    // classifies as Focused).
    let focused_button = gtk::ToggleButton::with_label("Focused");
    let other_button = gtk::ToggleButton::with_label("Other");
    other_button.set_group(Some(&focused_button));
    focused_button.set_active(true);
    let focused_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .tooltip_text("Focused Inbox")
        .build();
    focused_box.append(&focused_button);
    focused_box.append(&other_button);
    focused_box.set_visible(false);
    {
        let message_list = message_list.clone();
        focused_button.connect_toggled(move |button| {
            if button.is_active() {
                message_list.set_focused(crate::message_list::FocusedView::Focused);
            }
        });
    }
    {
        let message_list = message_list.clone();
        other_button.connect_toggled(move |button| {
            if button.is_active() {
                message_list.set_focused(crate::message_list::FocusedView::Other);
            }
        });
    }

    let message_header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(10)
        .margin_end(10)
        .margin_top(6)
        .margin_bottom(4)
        .build();
    message_header_row.append(&title_column);
    message_header_row.append(&focused_box);
    message_header_row.append(&favorite_button);
    message_header_row.append(&select_mode_button);
    message_header_row.append(&sync_button);
    message_header_row.append(&list_filter_button);
    message_header_row.append(&sort_direction_button);
    message_header_row.append(&sort_key_button);

    // The active search's refiner chips (roadmap 2.3): one removable pill per
    // operator in the query, shown above the list only while searching. Filled
    // by `refresh_search_refiners` from the parsed query.
    let search_refiner_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(10)
        .margin_end(10)
        .margin_bottom(4)
        .build();
    search_refiner_row.set_visible(false);

    let list_header = ListHeader {
        folder_label: folder_title_label,
        account_label: account_title_label,
        favorite_button: favorite_button.clone(),
        favorite_suppress: favorite_suppress.clone(),
        refiner_row: search_refiner_row.clone(),
        focused_box: focused_box.clone(),
        focused_button: focused_button.clone(),
        focused_available: Rc::new(Cell::new(false)),
        message_list: message_list.clone(),
    };
    refresh_list_header(&state, &list_header);

    // Secondary header: the message list's column names. Outlook shows these
    // only in its single-line columnar layout (UX-27); the three-line message
    // row (UX-28) has no columns to name, so the strip stays built but hidden
    // until that layout lands. Its geometry follows the old fixed sender /
    // subject / date columns and will need to be rebuilt on that layout's own
    // terms rather than reused as-is.
    let column_header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_end(10)
        .css_classes(["message-column-header"])
        .visible(false)
        .build();
    // Column names are chrome, not message content.
    crate::privacy::mark_ui(&column_header_row);
    // The sender column starts 51px in (3px accent + 8px margin + 32px avatar
    // + 8px margin); the box's own 8px spacing covers the last of that, so the
    // gutter is 43 wide.
    let avatar_gutter = gtk::Box::builder().width_request(43).build();
    column_header_row.append(&avatar_gutter);
    let sender_header = gtk::Label::builder().label("Sender").xalign(0.0).width_request(180).build();
    let subject_header = gtk::Label::builder().label("Subject").xalign(0.0).hexpand(true).build();
    let date_header = gtk::Label::builder().label("Date").xalign(1.0).build();
    column_header_row.append(&sender_header);
    column_header_row.append(&subject_header);
    column_header_row.append(&date_header);

    // --- Full-text search: a permanent `gtk::SearchEntry` in the window
    // header bar (see the header section below), wired up after the nav-rail
    // module buttons exist so a search started from any module lands on Mail.
    // Typing (debounced) swaps the list into search mode - instant results
    // from the local FTS index, with the open mailbox's live IMAP pass
    // catching up a beat later. Esc or clearing the text leaves search mode
    // and restores the previous view. The entry is built here, next to the
    // state it searches, and parented to the header later.
    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Search mail")
        .width_request(360)
        .tooltip_text("Search mail (Ctrl+F)")
        .css_classes(["header-search-entry"])
        .build();
    // A debounce token bumped on every keystroke and captured by each
    // scheduled timeout, so a timeout whose token is stale knows the user has
    // typed again and its query is superseded (the newer keystroke armed a
    // newer timeout).
    let search_debounce: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    state.borrow_mut().search_entry = Some(search_entry.clone());

    // The outbox banner (item 1.14): "N scheduled messages" (or waiting /
    // failed ones) above the list, opening the "Scheduled messages" dialog.
    // Revealed by `refresh_outbox_ui` whenever an `OutboxContents` arrives.
    let outbox_banner = adw::Banner::new("");
    outbox_banner.set_button_label(Some("View"));
    outbox_banner.set_revealed(false);
    {
        let state = state.clone();
        outbox_banner.connect_button_clicked(move |banner| present_outbox_dialog(&state, banner));
    }
    state.borrow_mut().outbox_banner = Some(outbox_banner.clone());

    // The automatic-replies banner (item 2.11 / M-24): "Automatic replies are
    // on …" above the list while any account has them active, with a Turn off
    // action. Revealed by `refresh_automatic_replies_ui`.
    let automatic_replies_banner = adw::Banner::new("");
    automatic_replies_banner.set_button_label(Some("Turn off"));
    automatic_replies_banner.set_revealed(false);
    {
        let state = state.clone();
        automatic_replies_banner.connect_button_clicked(move |_| turn_off_automatic_replies(&state));
    }
    state.borrow_mut().automatic_replies_banner = Some(automatic_replies_banner.clone());

    let message_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_box.append(&message_header_row);
    message_box.append(&outbox_banner);
    message_box.append(&automatic_replies_banner);
    message_box.append(&column_header_row);
    message_box.append(&search_refiner_row);
    message_box.append(&message_list_stack);
    let message_card = card_section(&message_box);
    message_card.add_css_class("card-flush-start");
    message_card.set_margin_start(0);
    // Halves the gap to the reading pane: that gap is this margin plus the
    // Paned separator (12px, see `data/styles/shell.css`) plus the reading
    // card's own start margin, so dropping both card margins to 0 takes it
    // from 24px to 12px. Zeroing the margins rather than narrowing the
    // separator keeps the whole gap draggable - the separator is
    // transparent, so the two look identical but only it grabs the pointer.
    message_card.set_margin_end(0);

    // --- Sync button -> re-sync whatever the list is showing. ---
    {
        let state = state.clone();
        sync_button.connect_clicked(move |_| resync_current_view(&state));
    }

    // --- Sort direction toggle -> flip the order and re-sort in place. The
    // re-sort reads the visible list back out of the model rather than
    // re-fetching, so it costs nothing and keeps the selection (see
    // `MessageListModel::displayed_messages`). ---
    {
        // Phase 5: put the button back on the persisted direction before the
        // handler below is wired, so startup doesn't write anything through.
        let descending = state.borrow().sort_descending;
        sort_direction_button.set_active(descending);
        sort_direction_button.set_icon_name(sort_direction_icon(descending));
        sort_direction_button.set_tooltip_text(Some(sort_direction_tooltip(state.borrow().sort_key, descending)));
    }
    {
        let state = state.clone();
        let message_list = message_list.clone();
        sort_direction_button.connect_toggled(move |button| {
            let descending = button.is_active();
            button.set_icon_name(sort_direction_icon(descending));
            button.set_tooltip_text(Some(sort_direction_tooltip(state.borrow().sort_key, descending)));
            state.borrow_mut().sort_descending = descending;
            state.borrow().settings.set_bool(crate::settings::SORT_DESCENDING, descending);
            resort_message_list(&state, &message_list);
        });
    }

    // --- Sort key -> a stateful action so the menu renders radio checks. The
    // action is added to the window once it exists (see `sort_key_action`
    // below); menu actions resolve through the widget hierarchy at activation
    // time, so registering it after the menu is built is fine. ---
    let sort_key_action = gio::SimpleAction::new_stateful("sort-key", Some(glib::VariantTy::STRING), &state.borrow().sort_key.action_state().to_variant());
    {
        let state = state.clone();
        let message_list = message_list.clone();
        let sort_key_button = sort_key_button.clone();
        let sort_direction_button = sort_direction_button.clone();
        sort_key_action.connect_activate(move |action, parameter| {
            let Some(key) = parameter.and_then(|p| p.str()).and_then(SortKey::from_action_state) else {
                return;
            };
            action.set_state(&key.action_state().to_variant());
            sort_key_button.set_label(key.label());
            sort_direction_button.set_tooltip_text(Some(sort_direction_tooltip(key, sort_direction_button.is_active())));
            state.borrow_mut().sort_key = key;
            state.borrow().settings.set_string(crate::settings::SORT_KEY, key.action_state());
            resort_message_list(&state, &message_list);
        });
    }

    // --- Filters -> one stateful boolean action per `Filter` (win.filter-*),
    // so the menu renders independent check marks. Toggling one updates the
    // model's set, relabels the button with the active count, and persists the
    // set in the `mail-filters` setting. ---
    let filter_actions: Vec<gio::SimpleAction> = Filter::ALL
        .into_iter()
        .map(|filter| {
            let action = gio::SimpleAction::new_stateful(&format!("filter-{}", filter.action_state()), None, &initial_filters.contains(filter).to_variant());
            let state = state.clone();
            let message_list = message_list.clone();
            let list_filter_button = list_filter_button.clone();
            // A boolean stateful action renders as a check item, and GTK's
            // menu toggles it through `change-state` (not `activate`).
            action.connect_change_state(move |action, requested| {
                let on = requested.and_then(|value| value.get::<bool>()).unwrap_or(false);
                action.set_state(&on.to_variant());
                let set = message_list.filter().with(filter, on);
                message_list.set_filter(set);
                list_filter_button.set_label(&filter_button_label(set));
                state.borrow().settings.set_strv(crate::settings::MAIL_FILTERS, set.to_states());
            });
            action
        })
        .collect();

    // --- Favorite star -> add/remove the open folder from the tree's
    // Favorites section. Session-only (see `UiState::favorites`). ---
    {
        let state = state.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let suppress = favorite_suppress.clone();
        favorite_button.connect_toggled(move |button| {
            if suppress.get() {
                return;
            }
            let Some(mailbox) = state.borrow().current_mailbox.clone() else { return };
            set_folder_favorite(&state, &mailbox, button.is_active());
            apply_favorite_visual(button, button.is_active());
            // The tree grows/loses a whole section, so it has to be rebuilt -
            // which swaps the model and drops the highlight. Put it back on the
            // folder the user is still looking at.
            rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
            if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                if let Some(index) = find_mailbox_index(&model, &mailbox) {
                    folder_selection.set_selected(index);
                }
            }
        });
    }

    // --- Folder rows' "⋯" menu (and Empty button) -> folder actions. ---
    install_folder_menu_handler(&menu_handler, &state, &folder_selection, &folder_scroller, &list_header, &worker);

    let compose_button = gtk::Button::from_icon_name("mail-message-new-symbolic");
    compose_button.set_tooltip_text(Some("New Message"));

    // Dev-only: load a raw .eml fixture straight into the reading pane,
    // bypassing IMAP entirely - for manually exercising render_body()
    // against test-fixtures/. Compiled out of release builds.
    #[cfg(debug_assertions)]
    let open_eml_button = gtk::Button::from_icon_name("document-open-symbolic");
    #[cfg(debug_assertions)]
    open_eml_button.set_tooltip_text(Some("Open .eml (debug)"));

    // --- Reading pane: WebKit for HTML, GtkTextView for plain text ---
    let _reading_stack = reading_stack.clone();
    let webkit_settings = webkit::Settings::new();
    webkit_settings.set_enable_javascript(false);
    webkit_settings.set_enable_developer_extras(false);
    // Deliberately *not* forced to `Never` here, unlike the other three
    // WebViews in this app: under WebKitGTK's software-rendering path,
    // navigating this long-lived, reused WebView straight from one
    // message's HTML to the next can render two different messages'
    // documents simultaneously blended together on screen - not just a
    // stale-frame timing artifact (a "clear" load-and-await sequence ahead
    // of the real content was tried and did not fix it), but an apparent
    // bug in WebKit's own software/threaded compositor when this policy is
    // forced. Left at the default (on-demand) policy, which only engages
    // accelerated compositing when a page actually needs it - plain mail
    // essentially never does - so this should cost little of the GPU-memory
    // win the `Never` policy was introduced for on the other WebViews.
    // webkit_settings.set_hardware_acceleration_policy(webkit::HardwareAccelerationPolicy::Never);
    // The "Switch message theme" toggle lives on the user content manager:
    // its override stylesheet is added/removed there so WebKit re-applies the
    // style to the document already on screen the moment the toggle flips
    // (the toggle handler also re-renders, see below). Built here, empty, so
    // the toggle's closure can arm it later.
    let user_content_manager = webkit::UserContentManager::new();
    let theme_override_sheet = webkit::UserStyleSheet::new(
        MESSAGE_THEME_OVERRIDE_CSS,
        webkit::UserContentInjectedFrames::TopFrame,
        webkit::UserStyleLevel::User,
        &[],
        &[],
    );
    let web_view = webkit::WebView::builder()
        .settings(&webkit_settings)
        .user_content_manager(&user_content_manager)
        .hexpand(true)
        .vexpand(true)
        .build();
    // The message body is text the GTK text selectors can't reach inside a
    // `WebView`, so privacy blur's "All text" mode blurs the whole view.
    crate::privacy::mark_blur_all(&web_view);
    install_web_view_zoom(&web_view);
    // Inline `cid:` image resolution: register a custom URI scheme so WebKit
    // asks us for the bytes behind `<img src="cid:...">` references instead
    // of failing the load. The handler callback fires on a WebKit worker
    // thread, so it must not touch `UiState` - it only forwards the request
    // to the main loop, which matches it against the rendered message's
    // inline parts and fetches the bytes on demand (`FetchAttachment`, served
    // from the flat-file cache on repeat visits).
    //
    // Deliberately *not* calling `WebKitSecurityManager::register_uri_scheme_
    // as_local` here - despite how it reads, "local" is a restriction, not a
    // grant: it means *other* non-local pages are forbidden from linking to
    // this scheme, not that this scheme is reachable from anywhere. The
    // message body is loaded via `load_html(html, None)`, which gives the
    // page a non-local origin - so marking "cid" local was actively cutting
    // the page's own image tags off from the scheme meant to serve them,
    // which is exactly why no inline image ever loaded. A bare
    // `register_uri_scheme` with no security-manager registration is what
    // makes the scheme reachable from any page, local or not.
    let (cid_tx, cid_rx) = async_channel::unbounded();
    if let Some(context) = web_view.context() {
        context.register_uri_scheme("cid", move |request| {
            let Some(cid) = request.path() else { return };
            let _ = cid_tx.send_blocking(CidSchemeRequest {
                request: SendWrapper(request.clone()),
                cid: cid.to_string(),
            });
        });
    }
    // The scheme handler's main-loop half: resolve each forwarded `cid:`
    // reference against the message currently on the reading pane and fetch
    // the matching part's bytes. This task outlives any single message, so
    // the resolution is always validated against `rendered_message`/`pending_cid`
    // at the moment the request lands.
    {
        let state_for_cid = state.clone();
        glib::spawn_future_local(async move {
            while let Ok(request) = cid_rx.recv().await {
                dispatch_cid_request(&state_for_cid, &request.cid, request.request.0);
            }
        });
    }
    // Block navigation *away* from the loaded message body (e.g. clicking a
    // link) - but NOT the initial programmatic `load_html()` call itself,
    // which also fires a NavigationAction decision. Distinguish the two via
    // `is_user_gesture()` (and `NavigationType::LinkClicked`, which is more
    // reliable across WebKitGTK versions): a real click is a user gesture,
    // `load_html()` is not. Getting this wrong (blocking unconditionally)
    // silently vetoes every load, which is exactly the "reading pane always
    // blank" bug this fixes - the WebView was never rendering anything
    // because its own initial content load was being cancelled before it
    // started. A clicked link is handed off to the system's default browser
    // instead of being dropped, and `target="_blank"` links - which WebKit
    // reports as a *new-window* decision rather than a navigation - are
    // routed to the browser by the `create` handler below.
    //
    // Remote *subresources* (tracker pixels, remote images/fonts, `<iframe>`s
    // pointing at outside URLs) are vetoed outright: they'd let external
    // servers reach into the pane, and - the reason this matters for speed -
    // `render_body` only reveals the page once WebKit reports `Finished`,
    // which the document's load event waits on. A slow remote host can hold
    // the reading pane blank for seconds; blocking these loads keeps the
    // reveal fast. `data:` (the body itself), `cid:` (inline parts, not yet
    // resolved but harmless) and `about:`/`file:` are all local and allowed.
    // Config → Mail's "Load images from the web" relaxes the response veto
    // for `image/*` subresources specifically; everything else (scripts,
    // fonts, iframes) stays blocked, and the veto still applies to the
    // navigation branch so an embedded remote `<iframe>` can't load either.
    // The preference lives in `UiState::load_remote_images` - the single
    // source of truth the Config toggle flips - and is re-read on every
    // decision, so the viewer always reflects the current setting.
    //
    // The external-content trust flow extends the same predicate per
    // message: `render_body` stashes the rendered message's sender in
    // `UiState::rendered_trust_sender`, and the sender's entry in
    // `UiState::trusted_senders` (an exact address or `@domain`, per
    // receiving account, persisted in the UI-state database) raises the
    // bar to the entry's `TrustLevel`. State is re-read on every decision
    // too, so trusting a sender while a message is open applies on the
    // next render. `render_body` resolves the same predicate once per
    // render and enforces it with a Content-Security-Policy meta tag
    // injected into the loaded HTML (see `message_content_security_policy`)
    // - WebKitGTK only delivers `decide-policy` *response* decisions for
    // the main frame, not for subresources, so the response veto below
    // can't be the gate that holds the block.
    let state_for_policy = state.clone();
    web_view.connect_decide_policy(move |view, decision, decision_type| {
        let uri_is_local = |uri: &str| -> bool {
            let scheme = uri.split(':').next().unwrap_or("");
            matches!(scheme, "data" | "cid" | "about" | "file")
        };
        match decision_type {
            webkit::PolicyDecisionType::NavigationAction => {
                let navigation = decision.downcast_ref::<webkit::NavigationPolicyDecision>().and_then(|d| d.navigation_action());
                // A real click on a link: block the in-pane navigation so the
                // message body stays on screen, but hand the target off to the
                // system's default browser instead of dropping it. Keyed on
                // the navigation type (`LinkClicked`) rather than
                // `is_user_gesture()` alone, which is unreliable across
                // WebKitGTK versions. `target="_blank"` links never arrive
                // here at all - WebKit reports them as a *new-window*
                // decision, handled below via the `create` signal.
                let is_link_click = navigation.as_ref().map(|a| a.navigation_type() == webkit::NavigationType::LinkClicked).unwrap_or(false);
                let is_user_gesture = navigation.as_ref().map(|a| a.is_user_gesture()).unwrap_or(false);
                if is_link_click || is_user_gesture {
                    if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                        // Item 6.3 / S-08: a link whose visible text names a
                        // different host gets a confirmation first.
                        let suspicious = state_for_policy.borrow().rendered_link_warnings.iter().any(|href| href == &uri);
                        if suspicious {
                            show_suspicious_link_dialog(view, &uri);
                        } else {
                            open_uri_in_default_browser(&uri);
                        }
                    }
                    decision.ignore();
                    return true;
                }
                // A programmatic navigation (e.g. an `<iframe>` embedded in
                // the message pointing at a remote URL) must not load. The
                // initial `load_html()` is a `data:` URL and passes below.
                if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                    if !uri_is_local(&uri) {
                        decision.ignore();
                        return true;
                    }
                }
            }
            webkit::PolicyDecisionType::NewWindowAction => {
                // `target="_blank"` links (and middle-click opens) arrive as
                // a new-window request, not a navigation. These bindings
                // can't read the target URL off a new-window decision, so
                // allow it through and let the `create` handler - connected
                // below - route the URL to the default browser and swallow
                // the window request.
                return false;
            }
            webkit::PolicyDecisionType::Response => {
                // Veto remote subresource responses (images, fonts, scripts)
                // so the page's load event isn't held hostage by external
                // servers. The main frame's own resource - the `data:` body
                // URL - is always let through. "Load images from the web"
                // (Config → Mail) allows remote `image/*` responses; the
                // external-content trust flow (the "Trust sender" banner)
                // additionally lets a trusted sender's message load images,
                // or - at the `AllContent` level - every remote subresource
                // response. Everything else stays blocked, and the veto
                // still applies to the navigation branch above.
                //
                // Note: WebKitGTK only emits *this* decision for the main
                // frame (verified empirically against 2.52) - subresource
                // responses never reach it. The real gate for remote
                // subresources is the Content-Security-Policy `render_body`
                // injects into every message body (see
                // `message_content_security_policy`); this branch is kept
                // as defense-in-depth for versions that do deliver it.
                if let Some(response) = decision.downcast_ref::<webkit::ResponsePolicyDecision>() {
                    if !response.is_main_frame_main_resource() {
                        if let Some(uri) = response.request().and_then(|r| r.uri()) {
                            if !uri_is_local(&uri) {
                                let is_image = response.response().and_then(|r| r.mime_type()).is_some_and(|m| m.starts_with("image/"));
                                let st = state_for_policy.borrow();
                                // The rendered message's sender (re-stashed
                                // by `render_body`) resolves its trust level;
                                // a `@domain` entry covers every address on
                                // that domain, and the strongest matching
                                // entry wins (an exact address outranks the
                                // account's broader domain entry).
                                let level = st.rendered_trust_sender.as_ref().and_then(|(account, sender)| {
                                    st.trusted_senders
                                        .iter()
                                        .filter(|((acc, entry), _)| acc == account && lookout_core::sender_matches_trust_entry(sender, entry))
                                        .map(|(_, level)| *level)
                                        .max()
                                });
                                let images_ok = st.load_remote_images || st.load_once_images || level.is_some_and(|l| l >= lookout_core::TrustLevel::Images);
                                let all_ok = level.is_some_and(|l| l >= lookout_core::TrustLevel::AllContent);
                                if !((images_ok && is_image) || all_ok) {
                                    decision.ignore();
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        false
    });

    // `target="_blank"` links (and middle-click / modifier-click opens) reach
    // this handler: the `NewWindowAction` decision above allowed them so the
    // URL - unreadable off the decision in these bindings - arrives here on
    // the navigation action. A link click routes the URL to the default
    // browser; the window request is swallowed either way (returning `None`
    // aborts the new-window creation, so nothing opens inside the pane).
    web_view.connect_create(move |_view, navigation_action| {
        let is_link_click = navigation_action.navigation_type() == webkit::NavigationType::LinkClicked;
        let is_user_gesture = navigation_action.is_user_gesture();
        if is_link_click || is_user_gesture {
            if let Some(uri) = navigation_action.request().and_then(|r| r.uri()) {
                open_uri_in_default_browser(&uri);
            }
        }
        None
    });

    let text_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let text_scroller = gtk::ScrolledWindow::builder().child(&text_view).build();

    // --- Reading-pane header: subject/sender/avatar/To/date, plus a second
    // set of Reply/Reply-All/Forward buttons duplicating the top command
    // toolbar's (see below). Lives inside the "message" page below, so it's
    // visible only while an actual message is shown, not for the empty
    // placeholder or the in-place composer.
    let message_header = crate::message_header::build();

    // The reading pane crossfades between three top-level pages: a single
    // "message" page that groups the message header with the body - so both
    // fade out/in together instead of the header popping out of sync with
    // the crossfading content - plus the "empty" placeholder and the
    // in-place composer. Inside the message page, a small no-transition
    // content stack toggles between the HTML web view and the plain-text
    // view without disturbing the outer crossfade.
    let reading_stack = gtk::Stack::new();
    let content_stack = gtk::Stack::new();
    content_stack.set_widget_name("body");
    content_stack.set_transition_type(gtk::StackTransitionType::None);
    content_stack.add_named(&web_view, Some("html"));
    content_stack.add_named(&text_scroller, Some("text"));
    let message_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_page.append(&message_header.subject_bar);
    message_page.append(&message_header.widget);
    // The attachment strip and body content stack are named children -
    // `find_named_child` locates them by walk rather than relying on sibling
    // order, so `action_bar` can sit after the body without breaking
    // `render_body`'s lookup. The strip is hidden (and emptied) when the
    // message has no attachments.
    let attachment_strip = gtk::Box::new(gtk::Orientation::Vertical, 6);
    attachment_strip.set_widget_name("attachments");
    attachment_strip.set_margin_start(12);
    attachment_strip.set_margin_end(12);
    attachment_strip.set_margin_top(8);
    attachment_strip.set_margin_bottom(4);
    attachment_strip.set_visible(false);
    message_page.append(&attachment_strip);
    // The iMIP invite-details card, between the attachments and the body.
    // The `adw::Banner` at the bottom of the page only fits a title and a
    // button, so this card carries what the invitation actually says - when,
    // where, who's organizing, and any description - with one row per detail
    // (`render_invite_card` shows/hides rows as the payload provides them).
    // Built once here as a named child; `render_body` repopulates it per
    // message, mirroring the attachment strip's lifecycle.
    let imip_invite_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["card"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    imip_invite_card.set_widget_name("imip-invite-card");
    imip_invite_card.set_visible(false);
    imip_invite_card.append(&invite_detail_row("imip-when-row", "imip-when", "When"));
    imip_invite_card.append(&invite_detail_row("imip-where-row", "imip-where", "Where"));
    imip_invite_card.append(&invite_detail_row("imip-organizer-row", "imip-organizer", "Organizer"));
    imip_invite_card.append(&invite_detail_row("imip-description-row", "imip-description", "Description"));
    // CAL-35 / 3.18: a "Conflicts with …" row and a mini day timeline, both
    // filled by `render_invite_card` from the cached calendar and hidden when
    // the invite's day clashes with nothing (or isn't loaded).
    imip_invite_card.append(&invite_detail_row("imip-conflict-row", "imip-conflict", "Conflicts with"));
    let imip_timeline = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    imip_timeline.set_widget_name("imip-timeline");
    imip_timeline.set_visible(false);
    imip_invite_card.append(&imip_timeline);
    // CAL-20 / 3.17: a Join button for an online meeting (Teams/Zoom/Meet).
    // `render_invite_card` sets its URL/label from the detected join URL and
    // hides it for an in-person event.
    let imip_join_button = gtk::LinkButton::builder()
        .label("Join")
        .uri("about:blank")
        .css_classes(["suggested-action"])
        .halign(gtk::Align::Start)
        .build();
    imip_join_button.set_widget_name("imip-join-button");
    imip_join_button.set_visible(false);
    imip_invite_card.append(&imip_join_button);
    message_page.append(&imip_invite_card);
    message_page.append(&content_stack);
    message_page.append(&message_header.action_bar);
    // The List-Unsubscribe banner, between the header and the body: revealed
    // by `render_body` when the message's headers offer an unsubscribe
    // action (RFC 2369 / one-click RFC 8058), hidden otherwise and once the
    // user dismisses it. A named child so `render_body` can find it.
    let unsubscribe_banner = adw::Banner::new("Unsubscribe from this mailing list?");
    unsubscribe_banner.set_widget_name("unsubscribe-banner");
    unsubscribe_banner.set_button_label(Some("Unsubscribe"));
    unsubscribe_banner.set_revealed(false);
    message_page.append(&unsubscribe_banner);
    // The iMIP banner (invitations / cancellations / RSVP replies carried as
    // `text/calendar` parts), between the header and the body like the
    // unsubscribe banner: revealed by `render_body` when the message carries
    // an iMIP payload the user hasn't dismissed, hidden otherwise. Its button
    // opens the per-method action dialog (REQUEST: Accept/Tentative/Decline,
    // CANCEL: remove-from-calendar confirm, REPLY: plain dismiss) - see the
    // handler registered once `calendar_state` exists below. The invite card
    // above carries the same three responses as split buttons with
    // "Edit response before sending" / "Don't send a response" variants
    // (CAL-15).
    let imip_banner = adw::Banner::new("Invitation");
    imip_banner.set_widget_name("imip-banner");
    imip_banner.set_button_label(Some("Respond…"));
    imip_banner.set_revealed(false);
    message_page.append(&imip_banner);
    // The external-content trust banner, between the header and the body
    // like the others: revealed by `render_body` when the message on screen
    // references remote content the load policy is blocking for its sender,
    // hidden otherwise and once the user acts on it. Its button opens the
    // trust dialog (load once / trust images / trust everything) - see the
    // handler registered below.
    let trust_banner = adw::Banner::new("Remote content is blocked");
    trust_banner.set_widget_name("trust-banner");
    trust_banner.set_button_label(Some("Trust sender…"));
    trust_banner.set_revealed(false);
    message_page.append(&trust_banner);
    // The sender-authentication banner (item 6.1 / S-03, M-35): revealed by
    // `render_body` when the message's `Authentication-Results` show a failure
    // or leave the sender unverified, or when the display name impersonates a
    // different address. No button - it is an advisory info bar (UX-33).
    let auth_banner = adw::Banner::new("");
    auth_banner.set_widget_name("auth-banner");
    auth_banner.set_revealed(false);
    message_page.append(&auth_banner);
    // The external-sender banner (item 6.2 / S-06): revealed when the sender's
    // domain is outside the configured internal-domain list. No button.
    let external_banner = adw::Banner::new("This sender is external");
    external_banner.set_widget_name("external-banner");
    external_banner.set_revealed(false);
    message_page.append(&external_banner);
    // The sensitivity banner (item 6.5 / C-06): revealed for a message whose
    // RFC 2156 `Sensitivity` is set (Personal / Private /
    // Company-Confidential). No button.
    let sensitivity_banner = adw::Banner::new("");
    sensitivity_banner.set_widget_name("sensitivity-banner");
    sensitivity_banner.set_revealed(false);
    message_page.append(&sensitivity_banner);
    // Link-safety status (item 6.3 / S-08): shows the real target of the link
    // under the pointer, so a misleading anchor text can't hide where a click
    // goes. Pinned at the bottom of the message page, hidden unless a link is
    // hovered.
    let link_status = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["dim-label"])
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(4)
        .visible(false)
        .build();
    link_status.set_widget_name("link-status");
    message_page.append(&link_status);
    {
        let link_status = link_status.clone();
        web_view.connect_mouse_target_changed(move |_view, hit_test, _modifiers| {
            if hit_test.context_is_link() {
                if let Some(uri) = hit_test.link_uri() {
                    link_status.set_text(&uri);
                    link_status.set_visible(true);
                    return;
                }
            }
            link_status.set_visible(false);
        });
    }
    // The read-receipt banner (RFC 8098 `Disposition-Notification-To`),
    // between the header and the body like the others: revealed by
    // `render_body` when the message on screen requests a read receipt and
    // the automatic policy is off (when it's on, the receipt is sent
    // silently instead). Its button sends the receipt - see the handler
    // registered below.
    let read_receipt_banner = adw::Banner::new("This message requests a read receipt");
    read_receipt_banner.set_widget_name("read-receipt-banner");
    read_receipt_banner.set_button_label(Some("Send read receipt"));
    read_receipt_banner.set_revealed(false);
    message_page.append(&read_receipt_banner);
    // The draft banner (item 1.12): revealed by `render_body` for a message
    // that is a saved draft - its `\Draft` flag, or its home in the account's
    // Drafts mailbox - so the preview offers an explicit way back into the
    // composer. Its button opens the draft for editing; see the handler
    // registered below.
    let draft_banner = adw::Banner::new("This is a draft");
    draft_banner.set_widget_name("draft-banner");
    draft_banner.set_button_label(Some("Edit"));
    draft_banner.set_revealed(false);
    message_page.append(&draft_banner);
    // The snooze bar (item 1.15): "Snoozed until Tue 08:00" over a message
    // waiting in the Scheduled folder, revealed by `render_body` from the
    // message's wake-time keyword. Unsnooze wakes it now (back to the Inbox).
    let snooze_banner = adw::Banner::new("");
    snooze_banner.set_widget_name("snooze-banner");
    snooze_banner.set_button_label(Some("Unsnooze"));
    snooze_banner.set_revealed(false);
    {
        let state = state.clone();
        snooze_banner.connect_button_clicked(move |_| {
            let Some((mailbox, uid, _)) = state.borrow().rendered_snooze.clone() else { return };
            let Some(account_id) = mailbox_account_id(&mailbox) else { return };
            send_account_command(&state, &account_id, AccountCommand::WakeSnoozed { mailbox, uids: vec![uid] });
        });
    }
    message_page.append(&snooze_banner);
    // "Follow up. Due by …" (item 1.16 / UX-33): the rendered message's
    // follow-up flag, with Mark complete (or Clear flag once complete).
    let follow_up_banner = adw::Banner::new("");
    follow_up_banner.set_widget_name("follow-up-banner");
    follow_up_banner.set_revealed(false);
    {
        let state = state.clone();
        let message_list = message_list.clone();
        follow_up_banner.connect_button_clicked(move |_| {
            let Some((mailbox, uid, flag)) = state.borrow().rendered_follow_up.clone() else {
                return;
            };
            let Some(summary) = message_list.all_messages().into_iter().find(|m| m.mailbox == mailbox && m.uid == uid) else {
                return;
            };
            let next = if flag.status == lookout_core::FlagStatus::Complete {
                crate::follow_up::cleared()
            } else {
                crate::follow_up::completed()
            };
            set_follow_up(&state, &message_list, vec![summary], next);
        });
    }
    message_page.append(&follow_up_banner);
    reading_stack.add_named(&message_page, Some("message"));
    let reading_empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reading_stack.add_named(&reading_empty, Some("empty"));
    // A separate page from "empty" (which several call sites already treat
    // as a true blank state) - "nothing selected" and "several messages
    // deliberately selected" are different states to the user even though
    // both mean "no single body to show," so this gets its own name rather
    // than overloading "empty" with a sometimes-visible label.
    let reading_multi_label = gtk::Label::builder()
        .css_classes(["dim-label"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    crate::privacy::mark_ui(&reading_multi_label);
    let reading_multi = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).vexpand(true).build();
    reading_multi.append(&reading_multi_label);
    reading_stack.add_named(&reading_multi, Some("multi"));
    // A spinner page distinct from "empty": "empty" is a true blank used for
    // the momentary crossfade between messages, whereas an uncached body fetch
    // can take a real round trip. Showing feedback here (rather than a blank
    // pane) lets the "message unavailable" page below read as a *failure*
    // instead of looking like the app simply has nothing to say.
    let reading_loading = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .spacing(12)
        .build();
    let reading_loading_spinner = gtk::Spinner::new();
    reading_loading_spinner.set_widget_name("body-loading-spinner");
    let reading_loading_label = gtk::Label::builder().label("Loading message…").css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&reading_loading_label);
    reading_loading.append(&reading_loading_spinner);
    reading_loading.append(&reading_loading_label);
    reading_stack.add_named(&reading_loading, Some("loading"));
    // The failure page: shown when an uncached body fetch can't complete -
    // the server is unreachable or the network is down. `show_body_fetch_error`
    // updates the description to name whichever of the two it looks like; the
    // Retry button re-dispatches the fetch for the message that failed.
    let reading_error_retry = gtk::Button::builder().label("Retry").halign(gtk::Align::Center).css_classes(["pill"]).build();
    reading_error_retry.set_widget_name("body-error-retry");
    let reading_error_page = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("Message unavailable")
        .description(body_fetch_error_description(true))
        .build();
    reading_error_page.set_child(Some(&reading_error_retry));
    reading_error_page.set_widget_name("body-error-page");
    reading_stack.add_named(&reading_error_page, Some("error"));
    // The conversation page (item 1.7): a selected conversation, or a
    // message inside one, shows here as stacked cards.
    let conversation_context = ConversationContext {
        state: state.clone(),
        worker: worker.clone(),
        message_header: message_header.clone(),
        profile_card: profile_card_provider.clone(),
        menu: MessageMenuContext {
            reading_stack: reading_stack.clone(),
            ..message_menu_context.clone()
        },
    };
    {
        let (conversation_ui, conversation_page) = build_conversation_ui(&conversation_context, &reading_stack);
        reading_stack.add_named(&conversation_page, Some("conversation"));
        state.borrow_mut().conversation_ui = Some(conversation_ui);
    }
    // "Back to conversation" on a message opened on its own from a card.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        message_header.back_button.connect_clicked(move |button| {
            button.set_visible(false);
            if state.borrow().rendered_conversation.is_none() {
                return;
            }
            clear_single_message_state(&state);
            reading_stack.set_visible_child_name("conversation");
        });
    }
    reading_stack.set_visible_child_name("empty");
    // Retry acts on whatever fetch most recently failed, re-arming the whole
    // dispatch (pending request + timeout + loading page).
    {
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        reading_error_retry.connect_clicked(move |_| {
            let Some((mailbox, uid)) = state.borrow().body_fetch_error.clone() else { return };
            request_body_fetch(&state, &worker, &reading_stack, &message_header, mailbox, uid);
        });
    }
    // Interpolated crossfade between the reading pane's pages so a
    // message's header + body fade out and the next fades in instead of
    // snapping. Message switches already pass through the "empty" page
    // (the selection handler flips there before the body arrives), so both
    // halves of the transition fire for free - `render_body` handles the
    // same-page re-render case by routing through "empty" explicitly.
    reading_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    reading_stack.set_transition_duration(100);
    // --- List-Unsubscribe banner actions. The banner's button acts on
    // whatever message is on the reading pane (its parsed unsubscribe info is
    // stashed in `UiState::unsubscribe_info` by `render_body`): a one-click
    // POST (RFC 8058) when the list advertises one, degrading to the
    // `mailto:` action when the POST fails or no URL is offered. Acting on
    // the offer dismisses the banner for that message (Adw.Banner has no
    // close button of its own); a failed POST with no mailto fallback keeps
    // it visible so the user can try again.
    {
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        unsubscribe_banner.connect_button_clicked(move |banner| {
            let (mailbox, uid, list, cmd_tx) = {
                let st = state.borrow();
                let (mailbox, uid) = match &st.rendered_message {
                    Some(rendered) => rendered.clone(),
                    None => return,
                };
                let Some(list) = st.unsubscribe_info.clone() else { return };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let Some(handle) = st.accounts.get(&account_id) else { return };
                (mailbox, uid, list, handle.cmd_tx.clone())
            };
            let dismiss = |state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid| {
                state.borrow_mut().unsubscribe_dismissed = Some((mailbox.clone(), uid));
            };
            if list.one_click {
                if let Some(url) = list.http.clone() {
                    // One-click POST: disable the button while in flight so a
                    // double-click can't send twice.
                    banner.set_sensitive(false);
                    let state_for_post = state.clone();
                    let worker_for_post = worker.clone();
                    let reading_stack_for_post = reading_stack.clone();
                    let banner_for_post = banner.clone();
                    glib::spawn_future_local(async move {
                        let result = post_one_click_unsubscribe(&url).await;
                        banner_for_post.set_sensitive(true);
                        match result {
                            Ok(()) => {
                                dismiss(&state_for_post, &mailbox, uid);
                                banner_for_post.set_revealed(false);
                                if let Some(overlay) = &state_for_post.borrow().toast_overlay {
                                    overlay.add_toast(adw::Toast::new("Unsubscribed"));
                                }
                            }
                            Err(message) => {
                                if let Some(overlay) = &state_for_post.borrow().toast_overlay {
                                    let title = glib::markup_escape_text(&format!("Couldn't unsubscribe: {message}"));
                                    overlay.add_toast(adw::Toast::new(&title));
                                }
                                // The list may still accept the RFC 2369
                                // mailto path - degrade to it rather than
                                // leaving the user with nothing.
                                if let Some(mailto) = list.mailto.clone() {
                                    dismiss(&state_for_post, &mailbox, uid);
                                    banner_for_post.set_revealed(false);
                                    open_mailto_unsubscribe(&state_for_post, &worker_for_post, &reading_stack_for_post, mailto, cmd_tx, mailbox_account_id(&mailbox));
                                }
                            }
                        }
                    });
                    return;
                }
            }
            if let Some(mailto) = list.mailto.clone() {
                dismiss(&state, &mailbox, uid);
                banner.set_revealed(false);
                open_mailto_unsubscribe(&state, &worker, &reading_stack, mailto, cmd_tx, mailbox_account_id(&mailbox));
            }
        });
    }
    // --- External-content trust banner actions. The banner's button acts on
    // the sender of whatever message is on the reading pane (stashed in
    // `UiState::rendered_trust_sender` by `render_body`): a three-way dialog
    // lets the user load the message's remote images just this once, trust
    // the sender's images, or trust all of the sender's remote content. Trust
    // is persisted in the UI-state database and written through to
    // `trusted_senders`; every action dismisses the banner for that message
    // (Adw.Banner has no close button of its own) and re-renders the body so
    // the new policy applies to the open message immediately.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        let message_list = message_list.clone();
        trust_banner.connect_button_clicked(move |banner| {
            let (mailbox, uid) = {
                let st = state.borrow();
                let (mailbox, uid) = match &st.rendered_message {
                    Some(rendered) => rendered.clone(),
                    None => return,
                };
                let Some((_account, _sender)) = st.rendered_trust_sender.clone() else { return };
                (mailbox, uid)
            };
            let dialog = adw::AlertDialog::builder()
                .heading("Remote content blocked")
                .body("This message references content hosted on remote servers, which Lookout blocks. You can load it once, or trust this sender to load it from now on.")
                .default_response("images")
                .close_response("cancel")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("once", "Load images once");
            dialog.add_response("images", "Trust sender (images only)");
            dialog.add_response("all", "Trust sender (all content)");
            let state_for_dialog = state.clone();
            let reading_stack_for_dialog = reading_stack.clone();
            let message_header_for_dialog = message_header.clone();
            let message_list_for_dialog = message_list.clone();
            let banner_for_dialog = banner.clone();
            dialog.connect_response(None, move |_dialog, response| {
                if response == "cancel" {
                    return;
                }
                {
                    let mut st = state_for_dialog.borrow_mut();
                    st.trust_banner_dismissed = Some((mailbox.clone(), uid));
                    st.load_once_images = response == "once";
                    if response == "images" || response == "all" {
                        let level = if response == "all" {
                            lookout_core::TrustLevel::AllContent
                        } else {
                            lookout_core::TrustLevel::Images
                        };
                        if let Some((account, sender)) = st.rendered_trust_sender.clone() {
                            if let Some(db) = &st.ui_db {
                                let _ = db.set_trusted_sender(&account, &sender, level);
                            }
                            st.trusted_senders.insert((account, sender), level);
                        }
                    }
                }
                banner_for_dialog.set_revealed(false);
                rerender_current_message(&state_for_dialog, &reading_stack_for_dialog, &message_header_for_dialog, &message_list_for_dialog);
            });
            dialog.present(Some(banner));
        });
    }
    // --- Read-receipt banner actions. The banner's button sends the RFC
    // 8098 receipt for whatever message is on the reading pane (the request
    // and context stashed by `render_body`): one click, one receipt, and the
    // message is marked receipted so re-opening it won't offer again - the
    // same per-message dismissal convention as the other banners.
    {
        let state = state.clone();
        read_receipt_banner.connect_button_clicked(move |banner| {
            let sent = send_read_receipt(&state, false);
            if sent {
                banner.set_revealed(false);
            }
        });
    }
    // --- Draft banner action (item 1.12): "Edit" opens the rendered draft in
    // the composer. The source is whatever `render_body` stashed for the
    // message currently on screen; the envelope comes from the selected row
    // (matching `(mailbox, uid)`, in case the selection has already moved on
    // under the banner's feet).
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_list = message_list.clone();
        draft_banner.connect_button_clicked(move |_| {
            let source = state.borrow().rendered_draft.clone();
            let Some((mailbox, uid)) = source else { return };
            // A scheduled send waiting in Drafts (item 1.14): the button is
            // "Cancel send", which withdraws it from the outbox; the
            // composer reopens on `OutboxCancelled`.
            let scheduled = {
                let st = state.borrow();
                st.rendered_draft_message_id.as_deref().and_then(|id| scheduled_item_for_draft(&st, &mailbox, id))
            };
            if let Some((account_id, item)) = scheduled {
                send_account_command(&state, &account_id, AccountCommand::CancelOutbox { id: item.id });
                return;
            }
            let Some(summary) = message_list.selected_summary().filter(|s| s.mailbox == mailbox && s.uid == uid) else {
                return;
            };
            // The banner stays revealed: the message page (and with it the
            // banner) is simply hidden while the loading page or composer
            // takes over, and shows again if the composer is cancelled.
            open_draft_composer(&state, &reading_stack, &summary);
        });
    }
    // WebKit paints asynchronously - revealing the HTML page while a fresh
    // body is still loading would show a blank/white page before the message
    // appears. So the body is loaded while the pane holds on "empty", and a
    // single persistent `load-changed` handler reveals the page only once the
    // load completes. `render_body` arms this with `pending_html_reveal`; the
    // selection handler disarms it whenever the user moves on, so a load
    // started for a stale message can never yank the pane back open (the
    // older per-render one-shot handlers instead fired on *any* Finished and
    // caused double reveals / stale emails appearing).
    let state_for_reveal = state.clone();
    let content_stack_for_reveal = content_stack.clone();
    let reading_stack_for_reveal = reading_stack.clone();
    web_view.connect_load_changed(move |_, event| {
        if event != webkit::LoadEvent::Finished {
            return;
        }
        let (armed, stuck_on_empty) = {
            let st = state_for_reveal.borrow();
            (
                st.pending_html_reveal,
                st.rendered_message.is_some() && reading_stack_for_reveal.visible_child_name().as_deref() == Some("empty"),
            )
        };
        // `stuck_on_empty` is a backstop for a scenario the selection
        // handler's same-message guard is meant to prevent, not the normal
        // path: `pending_html_reveal` disarmed by something other than a
        // genuine navigation to a different message, leaving this load's
        // `Finished` with nowhere to reveal to. If the pane is still on
        // "empty" and `rendered_message` (set by `render_body` right before
        // this load was issued) hasn't been overwritten by a newer render,
        // this Finished event is still the one worth revealing - the
        // alternative is a permanently blank pane, since WebKit only fires
        // `Finished` once per load.
        if armed || stuck_on_empty {
            tracing::debug!(armed, stuck_on_empty, "WebKit load finished; revealing reading pane");
            state_for_reveal.borrow_mut().pending_html_reveal = false;
            reveal_message_page(&reading_stack_for_reveal, &content_stack_for_reveal, "html");
        }
    });
    // Floor so the reading pane never gets squeezed down to something
    // unusably short if the window itself is resized very short - since
    // both Paneds here are horizontal, every pane always spans the full
    // window height, so this is really a minimum-window-height constraint
    // expressed on the pane that most needs it. Set directly on the Stack
    // (not on `text_scroller`'s child) because `Gtk.ScrolledWindow`
    // deliberately absorbs its child's size request instead of propagating
    // it - setting it here, on the Stack itself, is what actually reaches
    // the window's own size negotiation.
    reading_stack.set_size_request(-1, 300);

    let reading_pane_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    reading_pane_box.append(&reading_stack);

    let reading_card = card_section(&reading_pane_box);
    // See `message_card` above - the other half of the halved gap.
    reading_card.set_margin_start(0);

    // Keep the reading pane's card fully transparent while it's showing the
    // "no message selected" placeholder, so the window background image
    // shows straight through with no card tint - matching `.folder-pane`'s
    // translucency, but taken all the way to zero alpha since there's no
    // content to read against here. Driven off the stack's own signal
    // (rather than each `set_visible_child_name` call site) so every path
    // that flips back to "empty" - initial state above, `render_body`'s
    // fallback, and the reset on account disconnect - is covered for free.
    let update_reading_card_transparency = {
        let reading_card = reading_card.clone();
        move |stack: &gtk::Stack| {
            if stack.visible_child_name().as_deref() == Some("empty") {
                reading_card.add_css_class("reading-pane-transparent");
            } else {
                reading_card.remove_css_class("reading-pane-transparent");
            }
        }
    };
    update_reading_card_transparency(&reading_stack);
    reading_stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
        update_reading_card_transparency(stack);
    });

    // --- Resizable panes: folders | (messages | reading), each its own
    // rounded card. `resize_start_child(false)` keeps the sidebar-like
    // panes from silently growing when the window resizes; the reading
    // pane absorbs the extra space.
    // The reading pane sits to the right of the message list by default, but
    // the user can dock it below (Outlook's "Reading pane: Bottom"); then this
    // paned is vertical - messages on top, reading pane below - so its split
    // position and percentage are measured by height instead of width (see
    // `paned_axis_extent`).
    let reading_pane_position = crate::settings::reading_pane_position(&settings.get_string(crate::settings::LAYOUT_READING_PANE_POSITION));
    let reading_pane_orientation = match reading_pane_position {
        crate::settings::ReadingPanePosition::Bottom => gtk::Orientation::Vertical,
        crate::settings::ReadingPanePosition::Right => gtk::Orientation::Horizontal,
    };
    let messages_reading_paned = gtk::Paned::builder()
        .orientation(reading_pane_orientation)
        .start_child(&message_card)
        .end_child(&reading_card)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(368)
        .build();
    // Apply the persisted visibility here; the View ribbon's "Reading pane"
    // dropdown only re-applies it when the user picks a new position.
    reading_card.set_visible(settings.get_bool(crate::settings::LAYOUT_READING_PANE));
    // A bottom reading pane closes the half-gap to the message list on the
    // vertical edge (the cards' facing top/bottom margins), the way the side
    // layout already does on the horizontal edge.
    if reading_pane_orientation == gtk::Orientation::Vertical {
        reading_card.set_margin_top(0);
        message_card.set_margin_bottom(0);
    }
    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&folder_card)
        .end_child(&messages_reading_paned)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(220)
        .build();
    main_paned.add_css_class("seamless-paned");

    let status_page_as_widget: gtk::Widget = status_page.clone().upcast();
    let main_paned_as_widget: gtk::Widget = main_paned.clone().upcast();
    let root_stack = gtk::Stack::new();
    // GtkStack defaults to homogeneous sizing on both axes, so its overall
    // requested size tracks the largest natural size across *every* named
    // page, not just the visible one - meaning a hidden page's content
    // filling in (e.g. the calendar/contacts sidebars populating from a
    // background sync) reflows the visible page too, and everything beside
    // it (nav_rail included).
    root_stack.set_hhomogeneous(false);
    root_stack.set_vhomogeneous(false);
    root_stack.add_named(&status_page_as_widget, Some("empty"));
    root_stack.add_named(&main_paned_as_widget, Some("mail"));
    root_stack.set_visible_child_name("empty");

    let calendar_status_page = adw::StatusPage::builder()
        .icon_name("x-office-calendar-symbolic")
        .title("No Calendar Accounts")
        .description("Add an account with Calendar enabled in GNOME Online Accounts to see events here.")
        .build();
    let calendar_prefs = state.borrow().settings.calendar_preferences();
    // The zone every calendar surface shows times in (item 3.1), set before
    // anything calendar-related is built or rendered.
    crate::calendar_tz::set_display_zone(crate::calendar_tz::DisplayZone::from_setting(calendar_prefs.primary_tz));
    crate::message_list::set_week_start(calendar_prefs.week_start);
    let calendar_main = Rc::new(calendar_view::build_main(&calendar_prefs));
    let calendar_sidebar = calendar_view::build_sidebar(calendar_prefs.week_start);
    let calendar_sidebar_card = card_section(&calendar_sidebar.root);
    calendar_sidebar_card.add_css_class("folder-pane");
    let calendar_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&calendar_sidebar_card)
        .end_child(&calendar_main.root)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(260)
        .build();
    calendar_paned.add_css_class("seamless-paned");
    root_stack.add_named(&calendar_status_page, Some("calendar-empty"));
    root_stack.add_named(&calendar_paned, Some("calendar"));

    // --- Tasks view: a single card holding the grouped task list. Tasks come
    // from the same CalDAV accounts as the calendar (a `VTODO` per resource),
    // so "no calendar accounts" shows a status page like the calendar's.
    let tasks_status_page = adw::StatusPage::builder()
        .icon_name("view-task-symbolic")
        .title("No Calendar Accounts")
        .description("Add an account with Calendar enabled in GNOME Online Accounts to see tasks here.")
        .build();
    let tasks_view = Rc::new(crate::tasks_view::build_tasks_view(state.borrow().categories.clone()));
    let tasks_card = card_section(&tasks_view.root);
    tasks_card.add_css_class("folder-pane");
    root_stack.add_named(&tasks_status_page, Some("tasks-empty"));
    root_stack.add_named(&tasks_card, Some("tasks"));

    // --- Lookout view: the dashboard tab at the top of the nav rail. A
    // snapshot of every connected account - people most contacted, emails
    // by time of day, outstanding tasks, and upcoming events - fed from
    // the mail caches and the calendar state, so "no accounts" shows a
    // status page like the other modules' empty states.
    let lookout_status_page = adw::StatusPage::builder()
        .icon_name("view-grid-symbolic")
        .title("No Accounts Connected")
        .description("Connect an account in GNOME Online Accounts to see your dashboard here.")
        .build();
    let lookout_view = Rc::new(crate::lookout_view::build_lookout_view());
    let lookout_card = card_section(&lookout_view.root);
    lookout_card.add_css_class("folder-pane");
    root_stack.add_named(&lookout_status_page, Some("lookout-empty"));
    root_stack.add_named(&lookout_card, Some("lookout"));

    let contacts_status_page = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("No Contact Accounts")
        .description("Add an account with Contacts enabled in GNOME Online Accounts to see people here.")
        .build();

    let contacts_category_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    let contacts_category_scroller = gtk::ScrolledWindow::builder()
        .child(&contacts_category_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let contacts_left_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    contacts_left_box.set_margin_start(8);
    contacts_left_box.set_margin_end(8);
    contacts_left_box.set_margin_top(8);
    contacts_left_box.set_margin_bottom(8);
    contacts_left_box.append(&contacts_category_scroller);

    let contacts_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    let contacts_scroller = gtk::ScrolledWindow::builder()
        .child(&contacts_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let contacts_right_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    contacts_right_box.set_margin_start(8);
    contacts_right_box.set_margin_end(8);
    contacts_right_box.set_margin_top(8);
    contacts_right_box.set_margin_bottom(8);
    let contacts_heading_label = gtk::Label::builder().label("People").xalign(0.0).css_classes(["heading"]).build();
    contacts_right_box.append(&contacts_heading_label);
    // The People pane's search box (P-06): filters the visible bucket by
    // name, email or organisation. Wired once `refresh_contacts_ui` exists,
    // below.
    // `search-changed` waits for a 150 ms pause in typing (GTK's default,
    // spelled out here because the list rebuild depends on it), so a fast
    // typist repaints the list once rather than per keystroke.
    let contacts_search_entry = gtk::SearchEntry::builder()
        .search_delay(150)
        .placeholder_text("Search contacts")
        .tooltip_text("Search contacts")
        .css_classes(["contacts-search-entry"])
        .build();
    contacts_search_entry.set_hexpand(true);
    // The list's Filter/Sort control (UX-65), a ▾ menu beside the search box.
    // Its contents are built and wired further down, once `refresh_contacts_ui`
    // exists.
    let contacts_filter_button = gtk::Button::builder()
        .icon_name(themed_icon_name(&["funnel-symbolic", "view-sort-descending-symbolic", "view-list-symbolic"]))
        .tooltip_text("Filter and sort contacts")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let contacts_search_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    contacts_search_row.append(&contacts_search_entry);
    contacts_search_row.append(&contacts_filter_button);
    contacts_right_box.append(&contacts_search_row);
    contacts_right_box.append(&contacts_scroller);

    let contacts_left_card = card_section(&contacts_left_box);
    contacts_left_card.add_css_class("folder-pane");
    let contacts_right_card = card_section(&contacts_right_box);
    contacts_right_card.add_css_class("folder-pane");
    // The right-hand details pane (UX-65): the selected contact's photo, name,
    // actions and Overview / Contact info / Files / Emails tabs, refilled on
    // every selection change.
    let contacts_details = Rc::new(crate::contacts_details::ContactsDetailsPane::build());
    let contacts_details_card = card_section(&contacts_details.root);
    contacts_details_card.add_css_class("folder-pane");
    // Middle list | details, then the whole of that behind the bucket rail -
    // each divider is persisted separately.
    let contacts_content_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&contacts_right_card)
        .end_child(&contacts_details_card)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(360)
        .build();
    contacts_content_paned.add_css_class("seamless-paned");
    let contacts_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&contacts_left_card)
        .end_child(&contacts_content_paned)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(220)
        .build();
    contacts_paned.add_css_class("seamless-paned");
    root_stack.add_named(&contacts_status_page, Some("contacts-empty"));
    root_stack.add_named(&contacts_paned, Some("contacts"));

    // Built here (rather than down by the rest of the Config wiring below)
    // so `config_view.paned` exists in time to join the shared pane-width
    // persistence closure right below.
    let config_view = Rc::new(crate::config_view::build());

    // The one real title bar for the window - owns the actual
    // minimize/maximize/close buttons. The per-card header bars inside
    // `root_stack` are explicitly told not to show these (see
    // `card_section`), so there's exactly one set, not four.
    let window_header = adw::HeaderBar::new();
    // With the Lookout button moved to the nav rail (UX-14), the header's
    // start side is empty. Strict centring keeps the search cluster in the
    // true middle of the bar instead of letting the asymmetric end-side
    // controls pull it toward the start.
    window_header.set_centering_policy(adw::CenteringPolicy::Strict);
    // The centred search cluster (UX-11): a scope dropdown, the search entry,
    // and a "Refine" button that opens the advanced-search form. The title
    // widget is the header bar's centre slot, so this replaces the redundant
    // window title text.
    let scope_dropdown = gtk::DropDown::from_strings(&[
        SearchScope::Folder.label(),
        SearchScope::Subfolders.label(),
        SearchScope::Account.label(),
        SearchScope::All.label(),
    ]);
    scope_dropdown.set_selected(search_scope_index(state.borrow().search_scope));
    scope_dropdown.set_tooltip_text(Some("Search scope"));
    // Calendar's own scope (CAL-22): the title bar follows the active module,
    // so Calendar shows "All calendars" / "Checked calendars" instead of the
    // mail folder scopes.
    let calendar_scope_dropdown = gtk::DropDown::from_strings(&["All calendars", "Checked calendars"]);
    let calendar_scope_checked = state.borrow().settings.get_string(crate::settings::CALENDAR_SEARCH_SCOPE) == "checked";
    calendar_scope_dropdown.set_selected(if calendar_scope_checked { 1 } else { 0 });
    calendar_scope_dropdown.set_tooltip_text(Some("Calendar search scope"));
    let scope_stack = gtk::Stack::new();
    scope_stack.add_named(&scope_dropdown, Some("mail"));
    scope_stack.add_named(&calendar_scope_dropdown, Some("calendar"));
    scope_stack.set_visible_child_name("mail");
    let search_refine_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["view-list-symbolic", "view-continuous-symbolic"]))
        .tooltip_text("Refine search")
        .css_classes(["flat"])
        .build();
    let search_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    search_box.append(&scope_stack);
    search_box.append(&search_entry);
    search_box.append(&search_refine_button);
    window_header.set_title_widget(Some(&search_box));
    // The Lookout dashboard's button joins the nav rail below the core apps
    // (UX-14), where Outlook puts More apps. It stays in the rail buttons'
    // toggle group, so the module switch remains mutually exclusive, and gets
    // the same selected-pill and shortcut-tooltip treatment.
    let lookout_icon_image = nav_rail_image(
        "/io/github/gavindi/Lookout/icons/observatorium-1.svg",
        include_bytes!("../../../data/resources/icons/observatorium-1.svg"),
    );
    let lookout_view_button = gtk::ToggleButton::builder()
        .child(&lookout_icon_image)
        .css_classes(["flat", "nav-rail-button"])
        .tooltip_text("Lookout")
        .build();
    // The search cluster is the header's centred title widget (built above);
    // nothing to pack here.
    // The privacy toggle: cycles the tri-state privacy blur (Config →
    // Advanced → "Privacy blur") from anywhere in the app, docked at the
    // header's right end so a screenshot run is one click away. It is a global
    // control, not Mail-only, so unlike the ribbon toggles it stays enabled in
    // every module. A `ToggleButton` (not a plain button) so the checked
    // highlight shows while any blur is on; the icon and tooltip then say
    // which level is active and what the next click does (see `apply_blur_mode`
    // below). The Settings button sits to its right, directly left of the
    // window controls, and opens the Settings window as a modal dialog (see
    // its click handler below). `pack_end` stacks right-to-left, so the calls
    // are ordered rightmost first: on screen this reads open-.eml, privacy,
    // Settings.
    let header_privacy_toggle = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["view-reveal-symbolic", "view-conceal-symbolic"]))
        .css_classes(["flat"])
        .build();
    header_privacy_toggle.set_tooltip_text(Some("Blur sensitive information"));
    let settings_button = gtk::Button::from_icon_name("preferences-system-symbolic");
    settings_button.set_css_classes(&["flat"]);
    settings_button.set_tooltip_text(Some("Settings"));
    window_header.pack_end(&settings_button);
    window_header.pack_end(&header_privacy_toggle);
    #[cfg(debug_assertions)]
    window_header.pack_end(&open_eml_button);

    // --- Menu bar row (File, then the ribbon's tabs: Home, Send / Receive,
    // Folder, View, Help). File (Quit) is a `MenuButton` with a real
    // popover; the tabs are mutually-exclusive toggle buttons that switch
    // the ribbon content row below (see `view_toolbar_stack`). Home holds the
    // command toolbar, Send / Receive the account sync, Folder the open
    // folder's commands, View the pane-visibility layout toggles and Help
    // About. Mail-only: they're disabled while another module is active (see
    // the nav-rail handlers), matching the codebase's honest-disabled
    // convention.
    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));

    let file_button = gtk::MenuButton::builder().label("File").css_classes(["flat"]).menu_model(&file_menu).build();
    let ribbon_tab = |label: &str| gtk::ToggleButton::builder().label(label).css_classes(["flat", "ribbon-tab"]).build();
    let home_button = ribbon_tab("Home");
    home_button.set_active(true);
    let send_receive_button = ribbon_tab("Send / Receive");
    let folder_tab_button = ribbon_tab("Folder");
    let view_button = ribbon_tab("View");
    let help_button = ribbon_tab("Help");
    // The tab strip, which the nav-rail handlers enable and disable as one.
    let ribbon_tabs = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
    let ribbon_tab_buttons = [
        (&home_button, "home"),
        (&send_receive_button, "send-receive"),
        (&folder_tab_button, "folder"),
        (&view_button, "view"),
        (&help_button, "help"),
    ];
    for (button, _) in ribbon_tab_buttons {
        if button != &home_button {
            button.set_group(Some(&home_button));
        }
        ribbon_tabs.append(button);
    }

    // Which ribbon tab is active (the names in `ribbon_tab_buttons`) and which
    // nav-rail module is selected ("mail" | "calendar" | "tasks" | "contacts"
    // | "lookout") - together they decide the `view_toolbar_stack`'s visible
    // child (see `ribbon_stack_name`). The tab state persists across module
    // switches; the nav-rail handlers re-enable the tabs on Mail and disable
    // them elsewhere.
    let active_ribbon_tab: Rc<Cell<&'static str>> = Rc::new(Cell::new("home"));
    let current_module: Rc<Cell<&'static str>> = Rc::new(Cell::new("mail"));

    let menu_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).css_classes(["toolbar"]).build();
    menu_bar.append(&file_button);
    menu_bar.append(&ribbon_tabs);

    // --- Command toolbar row. `compose_button`, `reply_button`,
    // `reply_all_button`, `forward_button`, `delete_button`,
    // `archive_button`, `report_button`, `pin_button`, `snooze_button`, and
    // `more_button` (a menu of reading-pane extras like "Save as .eml…") are
    // backed by real functionality.
    let reply_button = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
    reply_button.set_tooltip_text(Some("Reply"));
    let reply_all_button = gtk::Button::from_icon_name("mail-reply-all-symbolic");
    reply_all_button.set_tooltip_text(Some("Reply All"));
    // Forward ▾ (UX-21, item 1.11 / C-13): the main part forwards the
    // selection - re-attaching the original's files and inline images - and
    // the arrow offers "Forward as attachment" (the whole message as a
    // `message/rfc822` part).
    let forward_button = adw::SplitButton::builder().icon_name("mail-forward-symbolic").dropdown_tooltip("Forward options").build();
    forward_button.set_tooltip_text(Some("Forward"));
    {
        let group = gio::SimpleActionGroup::new();
        let action_forward_as_attachment = gio::SimpleAction::new("forward-as-attachment", None);
        {
            let state = state.clone();
            let message_list = message_list.clone();
            action_forward_as_attachment.connect_activate(move |_, _| {
                if let Some(summary) = message_list.selected_summary() {
                    forward_as_attachment(&state, &summary);
                }
            });
        }
        group.add_action(&action_forward_as_attachment);
        forward_button.insert_action_group("forward", Some(&group));
        let forward_menu = gio::Menu::new();
        forward_menu.append(Some("Forward as attachment"), Some("forward.forward-as-attachment"));
        forward_button.set_menu_model(Some(&forward_menu));
    }
    let delete_button = gtk::Button::from_icon_name("user-trash-symbolic");
    delete_button.set_tooltip_text(Some("Delete"));
    let archive_button = gtk::Button::from_icon_name("mail-archive-symbolic");
    archive_button.set_tooltip_text(Some("Archive"));
    // Report ▾ (UX-05): the main part reports the selection as junk; the
    // arrow opens Report junk / Report phishing / Not junk. Report phishing
    // (item 6.7 / S-09) is a no-op when no reporting address is set.
    let report_button = adw::SplitButton::builder().icon_name("mail-mark-junk-symbolic").dropdown_tooltip("Report options").build();
    report_button.set_tooltip_text(Some("Report"));
    {
        let group = gio::SimpleActionGroup::new();
        let junk_action = gio::SimpleAction::new("junk", None);
        {
            let state = state.clone();
            let message_list = message_list.clone();
            junk_action.connect_activate(move |_, _| set_junk_selection(&state, &message_list, &message_list.selected_summaries(), true));
        }
        group.add_action(&junk_action);
        let phishing_action = gio::SimpleAction::new("phishing", None);
        {
            let state = state.clone();
            let message_list = message_list.clone();
            phishing_action.connect_activate(move |_, _| report_phishing_selection(&state, &message_list.selected_summaries()));
        }
        group.add_action(&phishing_action);
        let not_junk_action = gio::SimpleAction::new("not-junk", None);
        {
            let state = state.clone();
            let message_list = message_list.clone();
            not_junk_action.connect_activate(move |_, _| set_junk_selection(&state, &message_list, &message_list.selected_summaries(), false));
        }
        group.add_action(&not_junk_action);
        report_button.insert_action_group("report", Some(&group));
        let report_menu = gio::Menu::new();
        report_menu.append(Some("Report junk"), Some("report.junk"));
        report_menu.append(Some("Report phishing"), Some("report.phishing"));
        report_menu.append(Some("Not junk"), Some("report.not-junk"));
        report_button.set_menu_model(Some(&report_menu));
    }
    let sweep_button = gtk::Button::from_icon_name("edit-clear-all-symbolic");
    sweep_button.set_tooltip_text(Some("Sweep"));
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        sweep_button.connect_clicked(move |button| {
            let selected = message_list.selected_summaries();
            let Some(primary) = selected.first().cloned() else {
                if let Some(overlay) = &state.borrow().toast_overlay {
                    overlay.add_toast(adw::Toast::new("Select a message to sweep"));
                }
                return;
            };
            show_sweep_dialog(button.upcast_ref::<gtk::Widget>(), state.clone(), worker.clone(), message_list.clone(), primary);
        });
    }
    let pin_button = gtk::Button::from_icon_name(pin_icon_name(false));
    pin_button.set_tooltip_text(Some("Pin/Unpin"));
    // Flag ▾ (item 1.16 / M-10): the main part flags the selection (no due
    // date) or clears its flag; the arrow opens Outlook's due-date menu.
    // A follow-up flag, never Pin's `\Flagged` (UX-02). Its icon tracks the
    // selection via `refresh_flag_button`.
    let flag_button = adw::SplitButton::builder().icon_name(follow_up_icon_name(false)).dropdown_tooltip("Flag options").build();
    flag_button.set_tooltip_text(Some("Flag"));
    // Mark read/unread: no explicit toolbar action for this existed before -
    // only the implicit mark-as-read that opening a message already does.
    // Same aggregate-direction policy as Pin/Unpin: any unread message in
    // the selection means the action marks everything read; only when every
    // selected message is already read does it become "mark all unread."
    let mark_read_button = gtk::Button::from_icon_name(themed_icon_name(&["mail-mark-unread-symbolic", "mail-unread-symbolic", "emblem-ok-symbolic"]));
    // Categorize: a menu of the defined color tags, toggle-checked against
    // the selected messages. Its popover is rebuilt on every `show` so the
    // check states track whichever messages are selected when it opens.
    let categorize_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["tag-symbolic", "mail-mark-important-symbolic"]))
        .build();
    categorize_button.set_tooltip_text(Some("Categorize"));
    let categorize_popover = gtk::Popover::new();
    categorize_button.set_popover(Some(&categorize_popover));
    {
        let tags = tags.clone();
        let state = state.clone();
        let message_list = message_list.clone();
        categorize_popover.connect_show(move |popover| {
            let targets = message_list.selected_summaries();
            let boxed = build_tag_menu(&tags, &state, targets, &message_list);
            popover.set_child(Some(&boxed));
        });
    }
    let snooze_button = gtk::Button::from_icon_name("appointment-soon-symbolic");
    snooze_button.set_tooltip_text(Some("Snooze"));
    // Quick steps ▾ (item 2.6 / M-15): a menu of the user's named action
    // chains, run on the selection. Rebuilt on every `show` so each row's
    // enabled state tracks the live selection, like the Categorize menu.
    let quick_steps_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["system-run-symbolic", "media-playback-start-symbolic", "go-next-symbolic"]))
        .build();
    quick_steps_button.set_tooltip_text(Some("Quick steps"));
    let quick_steps_popover = gtk::Popover::new();
    quick_steps_button.set_popover(Some(&quick_steps_popover));
    {
        let state = state.clone();
        let message_list = message_list.clone();
        quick_steps_popover.connect_show(move |popover| {
            let menu = crate::quick_steps_view::build_quick_steps_menu(&state, &message_list, popover);
            popover.set_child(Some(&menu));
        });
    }
    // "More": the ribbon's trailing overflow menu. It is shared by every
    // module and tab (it lives outside the ribbon stack, so it stays pinned
    // at the right edge), and its content has two parts:
    //
    //   - the overflowed ribbon controls, moved in by `RibbonOverflow` and
    //     put straight back on their row when there is room again;
    //   - on the Mail Home command band only, the reading-pane extras
    //     ("Open in new window", "Save as .eml…", …), rebuilt on every
    //     `show` so their enabled state tracks the current selection.
    let more_button = gtk::MenuButton::builder().icon_name("view-more-symbolic").build();
    more_button.set_tooltip_text(Some("More"));
    let more_popover = gtk::Popover::new();
    let more_extras_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    more_extras_box.set_visible(false);
    let more_overflow_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    more_overflow_box.add_css_class("ribbon-overflow");
    more_overflow_box.set_visible(false);
    let more_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    more_separator.set_visible(false);
    let more_root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    more_root.append(&more_overflow_box);
    more_root.append(&more_separator);
    more_root.append(&more_extras_box);
    let more_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(480)
        .build();
    more_scroller.set_child(Some(&more_root));
    more_popover.set_child(Some(&more_scroller));
    more_button.set_popover(Some(&more_popover));

    // The Mail ribbon's commands that act on the selected messages through
    // the context menu's own dispatcher (`run_message_menu_action`), so each
    // behaves - and undoes - exactly as its menu entry does. Nothing
    // selected: a toast says so rather than the click doing nothing.
    let run_on_selection: SelectionActionFn = {
        let context = message_menu_context.clone();
        Rc::new(move |anchor, action| {
            let (targets, primary) = message_menu_targets(&context.message_list, MessageMenuTarget::Selection);
            if targets.is_empty() {
                if let Some(overlay) = &context.state.borrow().toast_overlay {
                    overlay.add_toast(adw::Toast::new("Select a message first"));
                }
                return;
            }
            run_message_menu_action(&context, anchor, &targets, primary.as_ref(), action);
        })
    };
    let selection_button = |icon: &str, tooltip: &str, action: crate::message_menu::MessageMenuAction| {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tooltip));
        let run_on_selection = run_on_selection.clone();
        button.connect_clicked(move |button| run_on_selection(button.upcast_ref(), action.clone()));
        button
    };
    // Respond ▸ Meeting: Reply with meeting (item 3.19 / C-20).
    let meeting_button = selection_button("appointment-new-symbolic", "Reply with meeting", crate::message_menu::MessageMenuAction::ReplyWithMeeting);
    // Respond ▸ Add Sender: wired below, once the contact editor's host exists
    // (the profile card's "Add to contacts").
    let add_sender_button = gtk::Button::from_icon_name("contact-new-symbolic");
    add_sender_button.set_tooltip_text(Some("Add sender to contacts"));
    // Move: the context menu's Move ▸ Other folder… picker.
    let move_button = selection_button(
        themed_icon_name(&["folder-move-symbolic", "go-jump-symbolic"]),
        "Move to folder",
        crate::message_menu::MessageMenuAction::MoveOther,
    );
    // Rules ▾: create a rule from the selected message, or manage them all.
    let rules_button = gtk::MenuButton::builder().icon_name("emblem-system-symbolic").tooltip_text("Rules").build();
    {
        let group = gio::SimpleActionGroup::new();
        let create = gio::SimpleAction::new("create-rule", None);
        {
            let run_on_selection = run_on_selection.clone();
            let anchor = rules_button.downgrade();
            create.connect_activate(move |_, _| {
                if let Some(anchor) = anchor.upgrade() {
                    run_on_selection(anchor.upcast_ref(), crate::message_menu::MessageMenuAction::CreateRule);
                }
            });
        }
        group.add_action(&create);
        let manage = gio::SimpleAction::new("manage-rules", None);
        {
            let state = state.clone();
            let anchor = rules_button.downgrade();
            manage.connect_activate(move |_, _| {
                if let Some(anchor) = anchor.upgrade() {
                    crate::rules_view::show_manage_rules_dialog(anchor.upcast_ref(), state.clone());
                }
            });
        }
        group.add_action(&manage);
        rules_button.insert_action_group("rules", Some(&group));
        let menu = gio::Menu::new();
        menu.append(Some("Create rule…"), Some("rules.create-rule"));
        menu.append(Some("Manage rules…"), Some("rules.manage-rules"));
        rules_button.set_menu_model(Some(&menu));
    }
    // Find ▸ Filter Email: the message list header's own filter menu.
    let filter_email_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["funnel-symbolic", "view-filter-symbolic", "edit-find-symbolic"]))
        .tooltip_text("Filter email")
        .menu_model(&list_filter_menu)
        .build();
    // Send/Receive All Folders, here and on the Send / Receive tab.
    let send_receive_all_button = |state: &Rc<RefCell<UiState>>| {
        let button = gtk::Button::from_icon_name("view-refresh-symbolic");
        button.set_tooltip_text(Some("Send/Receive all folders"));
        let state = state.clone();
        button.connect_clicked(move |_| send_receive_all(&state));
        button
    };
    let home_send_receive_button = send_receive_all_button(&state);

    // --- Command ribbon (Mail Home). Every control above was created and
    // wired already; the ribbon only dresses it with an icon and a label,
    // arranged in Outlook's groups (see `ribbon.rs`). The Simplified/Classic
    // layout is shared app-wide via `ribbon_style`.
    let command_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    command_band.group("New", |group| {
        group.button(&compose_button, "mail-message-new-symbolic", "New Message");
    });
    command_band.group("Delete", |group| {
        group.button(&delete_button, "user-trash-symbolic", "Delete");
        group.button(&archive_button, "mail-archive-symbolic", "Archive");
        group.small_split(&report_button, "mail-mark-junk-symbolic", "Report");
        group.small_button(&sweep_button, "edit-clear-all-symbolic", "Sweep");
    });
    command_band.group("Respond", |group| {
        group.button(&reply_button, "mail-reply-sender-symbolic", "Reply");
        group.button(&reply_all_button, "mail-reply-all-symbolic", "Reply all");
        group.split(&forward_button, "mail-forward-symbolic", "Forward");
        group.small_button(&meeting_button, "appointment-new-symbolic", "Meeting");
        group.small_button(&add_sender_button, "contact-new-symbolic", "Add Sender");
    });
    command_band.group("Quick Steps", |group| {
        group.menu(
            &quick_steps_button,
            themed_icon_name(&["system-run-symbolic", "media-playback-start-symbolic", "go-next-symbolic"]),
            "Quick Steps",
        );
    });
    command_band.group("Move", |group| {
        group.button(&move_button, themed_icon_name(&["folder-move-symbolic", "go-jump-symbolic"]), "Move");
        group.menu(&rules_button, "emblem-system-symbolic", "Rules");
    });
    let (pin_icon, flag_icon, mark_read_icon) = command_band.group("Tags", |group| {
        // The large commands first, so the Classic layout stacks Pin and
        // Snooze in a column after them.
        let read = group.button(
            &mark_read_button,
            themed_icon_name(&["mail-mark-unread-symbolic", "mail-unread-symbolic", "emblem-ok-symbolic"]),
            "Read",
        );
        group.menu(&categorize_button, themed_icon_name(&["tag-symbolic", "mail-mark-important-symbolic"]), "Categorize");
        let flag = group.split(&flag_button, follow_up_icon_name(false), "Flag");
        let pin = group.small_button(&pin_button, pin_icon_name(false), "Pin");
        group.small_button(&snooze_button, "appointment-soon-symbolic", "Snooze");
        (pin, flag, read)
    });
    command_band.group("Find", |group| {
        group.menu(
            &filter_email_button,
            themed_icon_name(&["funnel-symbolic", "view-filter-symbolic", "edit-find-symbolic"]),
            "Filter Email",
        );
    });
    command_band.group("Send/Receive", |group| {
        group.button(&home_send_receive_button, "view-refresh-symbolic", "Send/Receive All Folders");
    });
    // More is not part of this band: it is the trailing, always-visible
    // overflow button shared by every module (see `RibbonOverflow`).
    // The dynamic glyphs now that every control is dressed.
    refresh_pin_button(&pin_icon, &message_list, &state);
    refresh_flag_button(&flag_icon, &message_list);
    refresh_mark_read_button(&mark_read_icon, &message_list);

    let command_toolbar = command_band.root().clone();

    // --- Calendar's own command toolbar row, swapped in for the Mail one
    // (see `view_toolbar_stack` below) when the Calendar nav-rail button is
    // active. New Event opens the event editor (wired later, once
    // `calendar_state` exists); the segmented options (Day/Work
    // week/Week/Month/Schedule/Split/List) switch the main panel's stack;
    // Filter/Share remain disabled placeholders. Print is live (prints the displayed
    // range through WebKit's print pipeline).
    let new_event_button = gtk::Button::from_icon_name("appointment-new-symbolic");
    new_event_button.set_tooltip_text(Some("New Event"));

    let day_view_button = gtk::ToggleButton::builder().label("Day").build();
    let work_week_view_button = gtk::ToggleButton::builder().label("Work week").build();
    let week_view_button = gtk::ToggleButton::builder().label("Week").build();
    let month_view_button = gtk::ToggleButton::builder().label("Month").active(true).build();
    let schedule_view_button = gtk::ToggleButton::builder().label("Schedule").build();
    let split_view_button = gtk::ToggleButton::builder().label("Split view").build();
    let list_view_button = gtk::ToggleButton::builder().label("List").build();
    // One mutual-exclusion group so exactly one view is active at a time
    // (same `set_group` trick as the nav rail's Mail/Calendar buttons).
    schedule_view_button.set_group(Some(&month_view_button));
    split_view_button.set_group(Some(&month_view_button));
    list_view_button.set_group(Some(&month_view_button));
    week_view_button.set_group(Some(&month_view_button));
    work_week_view_button.set_group(Some(&week_view_button));
    day_view_button.set_group(Some(&work_week_view_button));
    for (button, view) in [
        (&day_view_button, "day"),
        (&work_week_view_button, "workweek"),
        (&week_view_button, "week"),
        (&month_view_button, "month"),
        (&schedule_view_button, "schedule"),
        (&split_view_button, "split"),
        (&list_view_button, "list"),
    ] {
        let calendar_main = calendar_main.clone();
        let state = state.clone();
        let search_entry = search_entry.clone();
        button.connect_toggled(move |btn| {
            if btn.is_active() {
                // Choosing a view is also a way out of the results page.
                if let Some(calendar_state) = active_calendar_state(&state) {
                    if calendar_state.borrow().calendar_search_active {
                        exit_calendar_search(&calendar_state, &calendar_main, &search_entry);
                    }
                }
                calendar_view::set_view(&calendar_main, view);
            }
        });
    }

    // CAL-22: the Calendar Filter action is live - it focuses the title-bar
    // calendar search. (Outlook's richer category/show-as/recurring filter
    // menu is Track U.7.)
    let filter_button = gtk::Button::from_icon_name("edit-find-symbolic");
    filter_button.set_tooltip_text(Some("Search calendar"));
    filter_button.set_sensitive(true);
    let share_button = gtk::Button::from_icon_name("send-to-symbolic");
    share_button.set_tooltip_text(Some("Share"));
    let print_button = gtk::Button::from_icon_name("printer-symbolic");
    print_button.set_tooltip_text(Some("Print"));
    let calendar_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    calendar_band.group("New", |group| {
        group.button(&new_event_button, "appointment-new-symbolic", "New Event");
    });
    // Go To: re-anchor every calendar view on today, step it back or
    // forward by its own unit (a day, a week, a month), or jump to a date.
    let calendar_today_button = gtk::Button::from_icon_name("go-home-symbolic");
    calendar_today_button.set_tooltip_text(Some("Go to today"));
    {
        let calendar_main = calendar_main.clone();
        calendar_today_button.connect_clicked(move |_| calendar_view::go_today(&calendar_main));
    }
    let calendar_step_button = |icon: &str, tooltip: &str, by: i64| {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tooltip));
        let calendar_main = calendar_main.clone();
        button.connect_clicked(move |_| calendar_view::step(&calendar_main, by));
        button
    };
    let calendar_back_button = calendar_step_button("go-previous-symbolic", "Back", -1);
    let calendar_forward_button = calendar_step_button("go-next-symbolic", "Forward", 1);
    let calendar_go_to_date_button = gtk::Button::from_icon_name("x-office-calendar-symbolic");
    calendar_go_to_date_button.set_tooltip_text(Some("Go to date"));
    {
        let calendar_main = calendar_main.clone();
        calendar_go_to_date_button.connect_clicked(move |button| {
            let calendar_main = calendar_main.clone();
            crate::datetime_picker::present_date_dialog(
                Some(button.upcast_ref()),
                "Go to date",
                "Date",
                "Go",
                calendar_view::anchor(&calendar_main),
                Rc::new(move |day| calendar_view::set_anchor(&calendar_main, day)),
            );
        });
    }
    calendar_band.group("Go To", |group| {
        group.button(&calendar_today_button, "go-home-symbolic", "Today");
        group.small_button(&calendar_back_button, "go-previous-symbolic", "Back");
        group.small_button(&calendar_forward_button, "go-next-symbolic", "Forward");
        group.small_button(&calendar_go_to_date_button, "x-office-calendar-symbolic", "Go to Date");
    });
    calendar_band.group("View", |group| {
        group.toggle(&day_view_button, themed_icon_name(&["view-calendar-day-symbolic", "x-office-calendar-symbolic"]), "Day");
        group.toggle(
            &work_week_view_button,
            themed_icon_name(&["view-calendar-workweek-symbolic", "x-office-calendar-symbolic"]),
            "Work week",
        );
        group.toggle(&week_view_button, themed_icon_name(&["view-calendar-week-symbolic", "x-office-calendar-symbolic"]), "Week");
        group.toggle(
            &month_view_button,
            themed_icon_name(&["view-calendar-month-symbolic", "x-office-calendar-symbolic"]),
            "Month",
        );
        group.small_toggle(&schedule_view_button, themed_icon_name(&["view-list-symbolic", "x-office-calendar-symbolic"]), "Schedule");
        group.small_toggle(&split_view_button, themed_icon_name(&["view-dual-symbolic", "sidebar-show-right-symbolic"]), "Split view");
        group.small_toggle(&list_view_button, themed_icon_name(&["view-list-symbolic"]), "List");
    });
    calendar_band.group("Filter", |group| {
        group.button(&filter_button, "edit-find-symbolic", "Filter");
    });
    calendar_band.group("Share", |group| {
        group.button(&share_button, "send-to-symbolic", "Share");
    });
    calendar_band.group("Print", |group| {
        group.button(&print_button, "printer-symbolic", "Print");
    });
    let calendar_command_toolbar = calendar_band.root().clone();

    // --- Tasks' own command toolbar row, swapped in when the Tasks nav-rail
    // button is active. New task opens the task editor (wired later, once
    // `calendar_state` exists); Connect Google Tasks runs the interactive
    // OAuth flow for the account's Google GOA account.
    let new_task_button = gtk::Button::from_icon_name(themed_icon_name(&["view-task-symbolic", "appointment-soon-symbolic"]));
    new_task_button.set_tooltip_text(Some("New Task"));
    let connect_google_tasks_button = gtk::Button::with_label("Connect Google Tasks");
    connect_google_tasks_button.set_tooltip_text(Some("Sign in to Google Tasks (the Tasks API, separate from Google's event-only CalDAV)"));

    // Sort/Group/Show-completed (T-09, UX-74): a menu of stateful `win.`
    // actions whose handlers (wired once `calendar_state` exists) persist to
    // GSettings and repaint the list.
    let tasks_arrange_menu = gio::Menu::new();
    let tasks_sort_section = gio::Menu::new();
    for (value, label) in crate::settings::TASK_SORT_KEYS.iter().zip(crate::settings::TASK_SORT_KEY_LABELS) {
        tasks_sort_section.append(Some(label), Some(&format!("win.task-sort('{value}')")));
    }
    tasks_arrange_menu.append_section(Some("Sort by"), &tasks_sort_section);
    let tasks_group_section = gio::Menu::new();
    for (value, label) in crate::settings::TASK_GROUP_BYS.iter().zip(crate::settings::TASK_GROUP_BY_LABELS) {
        tasks_group_section.append(Some(label), Some(&format!("win.task-group('{value}')")));
    }
    tasks_arrange_menu.append_section(Some("Group by"), &tasks_group_section);
    let tasks_completed_section = gio::Menu::new();
    tasks_completed_section.append(Some("Show completed"), Some("win.task-show-completed"));
    tasks_arrange_menu.append_section(None, &tasks_completed_section);
    let tasks_arrange_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["view-sort-ascending-symbolic", "view-list-symbolic"]))
        .tooltip_text("Sort and group")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&tasks_arrange_menu)
        .build();
    let task_sort_action = gio::SimpleAction::new_stateful(
        "task-sort",
        Some(glib::VariantTy::STRING),
        &settings.get_string(crate::settings::TASKS_SORT_KEY).to_variant(),
    );
    let task_group_action = gio::SimpleAction::new_stateful(
        "task-group",
        Some(glib::VariantTy::STRING),
        &settings.get_string(crate::settings::TASKS_GROUP_BY).to_variant(),
    );
    let task_show_completed_action = gio::SimpleAction::new_stateful("task-show-completed", None, &settings.get_bool(crate::settings::TASKS_SHOW_COMPLETED).to_variant());
    let tasks_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    tasks_band.group("New", |group| {
        group.button(&new_task_button, themed_icon_name(&["view-task-symbolic", "appointment-soon-symbolic"]), "New Task");
    });
    tasks_band.group("View", |group| {
        group.menu(
            &tasks_arrange_button,
            themed_icon_name(&["view-sort-ascending-symbolic", "view-list-symbolic"]),
            "Sort & group",
        );
    });
    tasks_band.group("Accounts", |group| {
        group.button(&connect_google_tasks_button, "x-office-calendar-symbolic", "Connect Google Tasks");
    });
    let tasks_command_toolbar = tasks_band.root().clone();

    // --- View tab's ribbon content (Mail module): a "Layout" group of
    // pane-visibility controls - Folder pane / Reading pane / Calendar
    // overview - and a "Messages" group. All three layout toggles default on;
    // their click handlers live in a later block (after every pane widget
    // exists), write through to GSettings (see `settings`), and are re-applied
    // from there at startup.
    let folder_pane_toggle = gtk::ToggleButton::builder().icon_name("folder-symbolic").build();
    folder_pane_toggle.set_tooltip_text(Some("Folder pane"));
    folder_pane_toggle.set_active(true);
    // Reading pane is a dropdown, not a plain toggle, because it has two
    // "on" positions (Outlook's "Reading pane: Right / Bottom") plus Off. It
    // drives the `win.reading-pane-position` stateful action; the activation
    // handler (which needs the paned and the pane-width scheduler) is wired
    // later, once those exist.
    let reading_pane_menu = gio::Menu::new();
    reading_pane_menu.append(Some("Right"), Some("win.reading-pane-position('right')"));
    reading_pane_menu.append(Some("Bottom"), Some("win.reading-pane-position('bottom')"));
    reading_pane_menu.append(Some("Off"), Some("win.reading-pane-position('off')"));
    let reading_pane_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["view-dual-symbolic", "sidebar-show-right-symbolic", "view-paged-symbolic"]))
        .tooltip_text("Reading pane")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&reading_pane_menu)
        .build();
    let reading_pane_action = gio::SimpleAction::new_stateful("reading-pane-position", Some(glib::VariantTy::STRING), &reading_pane_action_state(&settings).to_variant());
    // View ▸ Layout ▸ Density ▾ (UX-83, part of UX-22): Outlook's Full /
    // Medium / Compact row spacing. It drives the stateful `win.density`
    // action; the activation handler (which needs the two panes) is wired
    // below, beside Settings → Appearance's "Density" row, so the two surfaces
    // share one apply path and can never disagree. The state is normalised so
    // an unknown stored value starts on Medium, as the panes' CSS does.
    let initial_density = crate::settings::spacing_at(crate::settings::spacing_index(&settings.get_string(crate::settings::LAYOUT_SPACING)));
    let density_action = gio::SimpleAction::new_stateful("density", Some(glib::VariantTy::STRING), &initial_density.to_variant());
    let density_menu = gio::Menu::new();
    for (value, label) in crate::settings::SPACINGS.iter().zip(crate::settings::SPACING_LABELS) {
        density_menu.append(Some(label), Some(&format!("win.density('{value}')")));
    }
    let density_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["view-list-symbolic", "view-continuous-symbolic", "format-justify-fill-symbolic"]))
        .tooltip_text(format!("Density: {}", crate::settings::spacing_label(initial_density)))
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&density_menu)
        .build();
    let overview_pane_toggle = gtk::ToggleButton::builder().icon_name("x-office-calendar-symbolic").build();
    overview_pane_toggle.set_tooltip_text(Some("Calendar overview"));
    overview_pane_toggle.set_active(true);
    // View ▸ Messages ▾ (UX-22, item 1.7): Outlook's "Show as conversations /
    // individual messages" plus the conversation reading pane's order. Both
    // drive stateful `win.` actions whose handlers (wired below, once the
    // message list exists) persist to GSettings and mirror Config →
    // Appearance's rows.
    let messages_menu = gio::Menu::new();
    let organization_section = gio::Menu::new();
    organization_section.append(Some("Show as conversations"), Some("win.message-organization('conversations')"));
    organization_section.append(Some("Show as individual messages"), Some("win.message-organization('messages')"));
    messages_menu.append_section(None, &organization_section);
    let order_section = gio::Menu::new();
    for (value, label) in crate::settings::CONVERSATION_ORDERS.iter().zip(crate::settings::CONVERSATION_ORDER_LABELS) {
        order_section.append(Some(label), Some(&format!("win.conversation-order('{value}')")));
    }
    messages_menu.append_section(Some("Conversation reading pane"), &order_section);
    let messages_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["mail-unread-symbolic", "mail-read-symbolic", "mail-message-new-symbolic"]))
        .tooltip_text("Messages")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&messages_menu)
        .build();
    let message_organization_action = gio::SimpleAction::new_stateful(
        "message-organization",
        Some(glib::VariantTy::STRING),
        &message_organization_value(settings.get_bool(crate::settings::MAIL_THREADED)).to_variant(),
    );
    let conversation_order_action = gio::SimpleAction::new_stateful(
        "conversation-order",
        Some(glib::VariantTy::STRING),
        &crate::settings::conversation_order_at(crate::settings::conversation_order_index(&settings.get_string(crate::settings::MAIL_CONVERSATION_ORDER))).to_variant(),
    );
    // View ▸ Arrangement: the message list header's sort menu, and its
    // direction toggle flipped.
    let arrange_by_button = gtk::MenuButton::builder()
        .icon_name("view-sort-ascending-symbolic")
        .tooltip_text("Arrange by")
        .menu_model(&sort_key_menu)
        .build();
    let reverse_sort_button = gtk::Button::from_icon_name("view-sort-descending-symbolic");
    reverse_sort_button.set_tooltip_text(Some("Reverse sort"));
    {
        let sort_direction_button = sort_direction_button.clone();
        reverse_sort_button.connect_clicked(move |_| sort_direction_button.set_active(!sort_direction_button.is_active()));
    }
    // View ▸ Icons (UX-80): the optional Fluent pack, appended as its own
    // group. The default is the desktop's GNOME theme; the setting and the
    // Config → Appearance row are the same stateful `win.icon-style` action.
    let icon_style_menu = gio::Menu::new();
    for style in crate::icon_style::IconStyle::ALL {
        icon_style_menu.append(Some(style.label()), Some(&format!("win.icon-style('{}')", style.code())));
    }
    let icon_style_button = gtk::MenuButton::builder().tooltip_text("Icons").menu_model(&icon_style_menu).build();
    let view_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    view_band.group("Layout", |group| {
        group.toggle(&folder_pane_toggle, "folder-symbolic", "Folder pane");
        group.menu(
            &reading_pane_button,
            themed_icon_name(&["view-dual-symbolic", "sidebar-show-right-symbolic", "view-paged-symbolic"]),
            "Reading pane",
        );
        group.menu(
            &density_button,
            themed_icon_name(&["view-list-symbolic", "view-continuous-symbolic", "format-justify-fill-symbolic"]),
            "Density",
        );
        group.toggle(&overview_pane_toggle, "x-office-calendar-symbolic", "Calendar overview");
    });
    view_band.group("Messages", |group| {
        group.menu(
            &messages_button,
            themed_icon_name(&["mail-unread-symbolic", "mail-read-symbolic", "mail-message-new-symbolic"]),
            "Messages",
        );
    });
    view_band.group("Arrangement", |group| {
        group.menu(&arrange_by_button, "view-sort-ascending-symbolic", "Arrange By");
        group.button(&reverse_sort_button, "view-sort-descending-symbolic", "Reverse Sort");
    });
    view_band.group("Appearance", |group| {
        group.menu(
            &icon_style_button,
            themed_icon_name(&["applications-graphics-symbolic", "preferences-desktop-theme-symbolic", "view-grid-symbolic"]),
            "Icons",
        );
    });
    let view_toolbar = view_band.root().clone();

    // --- Send / Receive tab: sync every account now, and the accounts
    // themselves. Account Settings is wired below, once the Settings dialog
    // exists; Add Account opens GNOME Online Accounts, where Lookout's
    // accounts come from.
    let add_account_button = gtk::Button::from_icon_name("list-add-symbolic");
    add_account_button.set_tooltip_text(Some("Add account (GNOME Online Accounts)"));
    {
        let worker = worker.clone();
        add_account_button.connect_clicked(move |_| worker.spawn(crate::online_accounts::open_online_accounts()));
    }
    let account_settings_button = gtk::Button::from_icon_name("preferences-system-symbolic");
    account_settings_button.set_tooltip_text(Some("Account settings"));
    let send_receive_tab_button = send_receive_all_button(&state);
    let send_receive_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    send_receive_band.group("Send & Receive", |group| {
        group.button(&send_receive_tab_button, "view-refresh-symbolic", "Send/Receive All Folders");
    });
    send_receive_band.group("Accounts", |group| {
        group.button(&add_account_button, "list-add-symbolic", "Add Account");
        group.button(&account_settings_button, "preferences-system-symbolic", "Account Settings");
    });
    let send_receive_toolbar = send_receive_band.root().clone();

    // --- Folder tab: the open folder's own commands, run through the folder
    // pane's menu handler so each asks and behaves exactly as its right-click
    // entry does. A button is enabled as that menu would enable it for the
    // open folder - re-read when the tab is shown and when another folder is
    // chosen - and checks again when clicked.
    type FolderCommand = fn(&crate::folder_menu::FolderMenuState) -> Option<crate::folder_menu::FolderMenuAction>;
    let folder_command_button = |icon: &str, tooltip: &str| {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tooltip));
        button
    };
    let folder_move_icon = themed_icon_name(&["folder-move-symbolic", "go-jump-symbolic"]);
    let folder_commands: Vec<(gtk::Button, FolderCommand)> = {
        use crate::folder_menu::FolderMenuAction as F;
        vec![
            (folder_command_button("folder-new-symbolic", "New folder"), |m| m.create.then_some(F::CreateSubfolder)),
            (folder_command_button("document-edit-symbolic", "Rename folder"), |m| m.rename.then_some(F::Rename)),
            (folder_command_button(folder_move_icon, "Move folder"), |m| m.move_folder.then_some(F::Move)),
            (folder_command_button("user-trash-symbolic", "Delete folder"), |m| m.delete.then_some(F::Delete)),
            (folder_command_button("mail-read-symbolic", "Mark all as read"), |m| m.mark_read.then_some(F::MarkAllRead)),
            (folder_command_button("emblem-system-symbolic", "Run rules now"), |m| m.run_rules.then_some(F::RunRules)),
            (folder_command_button("edit-clear-all-symbolic", "Empty folder"), |m| m.empty.then_some(F::Empty)),
            (folder_command_button("starred-symbolic", "Show in Favorites"), |m| {
                Some(if m.is_favorite { F::RemoveFavorite } else { F::AddFavorite })
            }),
        ]
    };
    let refresh_folder_band: Rc<dyn Fn()> = {
        let state = state.clone();
        let buttons = folder_commands.clone();
        Rc::new(move || {
            let current = current_folder(&state);
            for (button, command) in &buttons {
                button.set_sensitive(current.as_ref().is_some_and(|(_, allowed)| command(allowed).is_some()));
            }
            // The favourites button says which way it goes.
            if let (Some((button, _)), Some((_, allowed))) = (buttons.last(), current.as_ref()) {
                button.set_tooltip_text(Some(if allowed.is_favorite { "Remove from Favorites" } else { "Show in Favorites" }));
            }
        })
    };
    for (button, command) in &folder_commands {
        let state = state.clone();
        let menu_handler = menu_handler.clone();
        let refresh_folder_band = refresh_folder_band.clone();
        let command = *command;
        button.connect_clicked(move |button| {
            let handler = menu_handler.borrow().clone();
            let target = current_folder(&state).and_then(|(mailbox, allowed)| command(&allowed).map(|action| (action, mailbox)));
            match (handler, target) {
                (Some(handler), Some((action, mailbox))) => handler(action, mailbox, button.clone().upcast()),
                _ => {
                    if let Some(overlay) = &state.borrow().toast_overlay {
                        overlay.add_toast(adw::Toast::new("Not available for this folder"));
                    }
                }
            }
            refresh_folder_band();
        });
    }
    let folder_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    if let [(new_folder, _), (rename, _), (move_folder, _), (delete, _), (mark_read, _), (run_rules, _), (empty, _), (favorite, _)] = folder_commands.as_slice() {
        folder_band.group("New", |group| {
            group.button(new_folder, "folder-new-symbolic", "New Folder");
        });
        folder_band.group("Actions", |group| {
            group.button(rename, "document-edit-symbolic", "Rename Folder");
            group.button(move_folder, folder_move_icon, "Move Folder");
            group.button(delete, "user-trash-symbolic", "Delete Folder");
        });
        folder_band.group("Clean Up", |group| {
            group.button(mark_read, "mail-read-symbolic", "Mark All as Read");
            group.button(run_rules, "emblem-system-symbolic", "Run Rules Now");
            group.button(empty, "edit-clear-all-symbolic", "Empty Folder");
        });
        folder_band.group("Favorites", |group| {
            group.button(favorite, "starred-symbolic", "Show in Favorites");
        });
    }
    let folder_toolbar = folder_band.root().clone();
    {
        let refresh_folder_band = refresh_folder_band.clone();
        folder_toolbar.connect_map(move |_| refresh_folder_band());
    }
    {
        // After the selection's own handlers have opened the folder.
        let refresh_folder_band = refresh_folder_band.clone();
        folder_selection.connect_selected_notify(move |_| {
            let refresh_folder_band = refresh_folder_band.clone();
            glib::idle_add_local_once(move || refresh_folder_band());
        });
    }

    // --- Help tab.
    let about_button = gtk::Button::from_icon_name("help-about-symbolic");
    about_button.set_tooltip_text(Some("About Lookout"));
    about_button.connect_clicked(|button| {
        let _ = button.activate_action("app.about", None);
    });
    let help_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    help_band.group("Help", |group| {
        group.button(&about_button, "help-about-symbolic", "About Lookout");
    });
    let help_toolbar = help_band.root().clone();

    let view_toolbar_stack = gtk::Stack::new();
    view_toolbar_stack.add_named(&command_toolbar, Some("mail-home"));
    view_toolbar_stack.add_named(&send_receive_toolbar, Some("mail-send-receive"));
    view_toolbar_stack.add_named(&folder_toolbar, Some("mail-folder"));
    view_toolbar_stack.add_named(&view_toolbar, Some("mail-view"));
    view_toolbar_stack.add_named(&help_toolbar, Some("mail-help"));
    view_toolbar_stack.add_named(&calendar_command_toolbar, Some("calendar"));
    view_toolbar_stack.add_named(&tasks_command_toolbar, Some("tasks"));

    // The contextual "Search" ribbon row (UX-11, roadmap 2.3): revealed while a
    // search is active (see `start_search`), it mirrors the title-bar scope
    // dropdown plus the Refine form and a Clear action.
    let ribbon_scope_dropdown = gtk::DropDown::from_strings(&[
        SearchScope::Folder.label(),
        SearchScope::Subfolders.label(),
        SearchScope::Account.label(),
        SearchScope::All.label(),
    ]);
    ribbon_scope_dropdown.set_selected(search_scope_index(state.borrow().search_scope));
    ribbon_scope_dropdown.set_tooltip_text(Some("Search scope"));
    let ribbon_refine_button = gtk::MenuButton::builder().label("Refine").css_classes(["flat"]).build();
    let search_clear_button = gtk::Button::with_label("Clear search");
    let search_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    search_band.group("Search", |group| {
        group.widget(&ribbon_scope_dropdown);
        group.menu(&ribbon_refine_button, "edit-find-symbolic", "Refine");
        group.button(&search_clear_button, "edit-clear-symbolic", "Clear search");
    });
    let search_toolbar = search_band.root().clone();
    view_toolbar_stack.add_named(&search_toolbar, Some("search"));
    state.borrow_mut().search_ribbon = Some(view_toolbar_stack.clone());

    // People's command ribbon. The buttons were created above; here they are
    // dressed with icons and labels and arranged in Outlook's groups.
    let new_contact_button = gtk::Button::with_label("New contact");
    // Edit/Delete for the contacts checked in the list, gated by
    // `refresh_contacts_actions`: Edit enables only when exactly one contact
    // is selected, Delete for any number; both start disabled.
    let contacts_edit_button = gtk::Button::with_label("Edit");
    contacts_edit_button.set_sensitive(false);
    let contacts_delete_button = gtk::Button::with_label("Delete");
    contacts_delete_button.set_sensitive(false);
    // Communicate ▸ Email: a new message to the one selected contact, gated
    // with Edit (and on the contact having an address).
    let contacts_email_button = gtk::Button::with_label("Email");
    contacts_email_button.set_sensitive(false);
    let import_contacts_button = gtk::Button::with_label("Import…");
    // Export is a split menu button (UX-16's split-button pattern): each
    // entry opens the save dialog in its format - the multi-card vCard, or
    // Outlook's CSV columns (P-08, §4.6).
    let export_contacts_menu = gio::Menu::new();
    export_contacts_menu.append(Some("vCard (.vcf)…"), Some("contacts-export.vcard"));
    export_contacts_menu.append(Some("Outlook CSV (.csv)…"), Some("contacts-export.csv"));
    let export_contacts_popover = gtk::PopoverMenu::from_model(Some(&export_contacts_menu));
    let export_contacts_button = gtk::MenuButton::builder()
        .label("Export")
        .tooltip_text("Export contacts")
        .popover(&export_contacts_popover)
        .build();
    let manage_groups_button = gtk::Button::with_label("Manage groups…");
    let open_contacts_window_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popout1", "window-new-symbolic", "view-restore-symbolic"]));
    open_contacts_window_button.set_tooltip_text(Some("Open the People screen in its own window"));
    let contacts_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    contacts_band.group("New", |group| {
        group.button(&new_contact_button, "contact-new-symbolic", "New contact");
    });
    contacts_band.group("Communicate", |group| {
        group.button(&contacts_email_button, "mail-message-new-symbolic", "Email");
    });
    contacts_band.group("Actions", |group| {
        group.button(&contacts_edit_button, "document-edit-symbolic", "Edit");
        group.button(&contacts_delete_button, "user-trash-symbolic", "Delete");
    });
    contacts_band.group("Transfer", |group| {
        group.button(&import_contacts_button, "document-open-symbolic", "Import…");
        group.menu(&export_contacts_button, "document-save-symbolic", "Export");
    });
    contacts_band.group("Groups", |group| {
        group.button(&manage_groups_button, "view-list-symbolic", "Manage groups…");
    });
    contacts_band.standalone().button(
        &open_contacts_window_button,
        crate::window::themed_icon_name(&["popout1", "window-new-symbolic", "view-restore-symbolic"]),
        "Open in window",
    );
    let contacts_command_toolbar = contacts_band.root().clone();
    view_toolbar_stack.add_named(&contacts_command_toolbar, Some("contacts"));

    // The Lookout dashboard's command toolbar: a single Refresh button that
    // re-reads the caches and widens the calendar sync horizon so upcoming
    // events reach into next month.
    let lookout_refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
    lookout_refresh_button.set_tooltip_text(Some("Refresh dashboard"));
    let lookout_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    lookout_band.group("Dashboard", |group| {
        group.button(&lookout_refresh_button, "view-refresh-symbolic", "Refresh");
    });
    let lookout_command_toolbar = lookout_band.root().clone();
    view_toolbar_stack.add_named(&lookout_command_toolbar, Some("lookout"));

    // The action-bar slot switcher, stored in `UiState` (not captured by the
    // composer-opening closures) because the message-row factories are built
    // earlier in this function than the toolbar row itself - and the
    // factories' reply button opens composers. `show_composer_in_reading_pane`
    // reads it from state and swaps the "mail-home" slot to the composer's
    // editor toolbar while it's embedded, back to the command toolbar when
    // the composer pops out or closes.
    state.borrow_mut().compose_action_bar = Some(Rc::new(ComposeActionBar {
        stack: view_toolbar_stack.clone(),
        command_toolbar: command_toolbar.clone(),
    }));

    // --- View-switcher rail: a narrow, deliberately unstyled (no `.card`,
    // no background) strip along the window's left edge so the background
    // image shows straight through it. The app's five modules -
    // Mail/Calendar/People/Tasks, then the Lookout dashboard where Outlook
    // puts More apps - share one toggle group for mutual-exclusive selection
    // and the `nav-rail-button` accent-pill styling (UX-14).
    let mail_icon_image = nav_rail_image("/io/github/gavindi/Lookout/icons/email-1.svg", include_bytes!("../../../data/resources/icons/email-1.svg"));
    let mail_view_button = gtk::ToggleButton::builder()
        .child(&mail_icon_image)
        .css_classes(["flat", "nav-rail-button"])
        .tooltip_text("Mail")
        .active(true)
        .build();
    let calendar_icon_image = nav_rail_image(
        "/io/github/gavindi/Lookout/icons/calendar-1.svg",
        include_bytes!("../../../data/resources/icons/calendar-1.svg"),
    );
    let calendar_view_button = gtk::ToggleButton::builder()
        .child(&calendar_icon_image)
        .css_classes(["flat", "nav-rail-button"])
        .tooltip_text("Calendar")
        .build();
    calendar_view_button.set_group(Some(&mail_view_button));
    let contacts_icon_image = nav_rail_image(
        "/io/github/gavindi/Lookout/icons/contact-1.svg",
        include_bytes!("../../../data/resources/icons/contact-1.svg"),
    );
    let contacts_view_button = gtk::ToggleButton::builder()
        .child(&contacts_icon_image)
        .css_classes(["flat", "nav-rail-button"])
        .tooltip_text("People")
        .build();
    contacts_view_button.set_group(Some(&calendar_view_button));
    let tasks_icon_image = nav_rail_image("/io/github/gavindi/Lookout/icons/task-1.svg", include_bytes!("../../../data/resources/icons/task-1.svg"));
    let tasks_view_button = gtk::ToggleButton::builder()
        .child(&tasks_icon_image)
        .css_classes(["flat", "nav-rail-button"])
        .tooltip_text("Tasks")
        .build();
    tasks_view_button.set_group(Some(&contacts_view_button));
    // The Lookout button was built above (next to the header's search
    // cluster); joining it to the rail buttons' toggle group keeps the tab
    // switching exclusive.
    lookout_view_button.set_group(Some(&mail_view_button));

    // `vexpand(true)` so the rail stretches the window's full height (it
    // sits beside `outer_toolbar_view` - header bar, menu bar, and command
    // toolbar included - rather than below those top bars). The content is a
    // fixed-height Box, wrapped in a scrolled window so every rail button
    // stays reachable at any window height. (Settings used to anchor at the
    // rail's bottom; it now lives in the header bar as a modal dialog.)
    let nav_rail_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(56)
        .margin_top(6)
        .margin_start(6)
        .spacing(6)
        .vexpand(true)
        .build();
    nav_rail_content.append(&mail_view_button);
    nav_rail_content.append(&calendar_view_button);
    nav_rail_content.append(&contacts_view_button);
    nav_rail_content.append(&tasks_view_button);
    // A little separation from the core apps, like Outlook's More apps
    // section (UX-14).
    lookout_view_button.set_margin_top(6);
    nav_rail_content.append(&lookout_view_button);
    let nav_rail = gtk::ScrolledWindow::builder()
        .child(&nav_rail_content)
        .has_frame(false)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .overlay_scrolling(true)
        .vexpand(true)
        .build();

    // --- Mail-screen calendar overview pane: a mini month-picker + a list
    // of the clicked day's events and every outstanding task, docked to the
    // far right of the window, spanning the same full height as `nav_rail`
    // (it's a sibling in `window_body`, not nested inside `root_stack`).
    // Mail-only - the Calendar view already has its own full sidebar with a
    // mini-calendar.
    let mail_calendar_overview = calendar_view::build_mini(calendar_prefs.week_start);
    // Half-width day cells (see `.mini-calendar-compact` in
    // `data/styles/calendar.css`). The day buttons' own natural size is what set
    // the pane's real width - the `width_request` below is only a floor - so
    // narrowing the pane means narrowing the buttons.
    mail_calendar_overview.root.add_css_class("mini-calendar-compact");
    let mail_overview_day_list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(8).build();
    // Matches `build_sidebar()`'s own width_request - without an explicit
    // cap here, the mini-calendar's day-button grid requests its natural
    // (much wider) size instead of a compact peek-pane width.
    let mail_overview_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(140)
        .margin_top(2)
        .margin_bottom(2)
        .margin_start(2)
        .margin_end(2)
        .build();
    mail_overview_box.append(&mail_calendar_overview.root);
    mail_overview_box.append(&mail_overview_day_list);

    let mail_calendar_overview_card = card_section(&mail_overview_box);
    mail_calendar_overview_card.add_css_class("folder-pane");
    mail_calendar_overview_card.set_vexpand(true);
    // True while the window is too narrow to show the whole overview pane:
    // the pane is auto-hidden then instead of letting the paned clip it, and
    // stays hidden until a resize makes it fit again (the toggle handler and
    // the mail-tab handler both consult this flag; see `check_overview_fits`).
    let overview_forced_hidden: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // --- Ribbon tab strip + View-tab pane toggles. Home/View swap the
    // ribbon content row (`view_toolbar_stack`) between Mail's command
    // toolbar and the Layout toggles; the three toggles show/hide their
    // pane. Home/View only make sense on the Mail module - the Calendar/
    // Config nav handlers disable them below, so the tab buttons can't be
    // clicked there (and the `if btn.is_active()` guard makes re-entrant
    // toggles no-ops anyway).
    for (button, tab) in ribbon_tab_buttons {
        let view_toolbar_stack = view_toolbar_stack.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        button.connect_toggled(move |btn| {
            if btn.is_active() {
                active_ribbon_tab.set(tab);
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name(current_module.get(), tab));
            }
        });
    }
    {
        let folder_card = folder_card.clone();
        let state = state.clone();
        folder_pane_toggle.connect_toggled(move |btn| {
            state.borrow().settings.set_bool(crate::settings::LAYOUT_FOLDER_PANE, btn.is_active());
            folder_card.set_visible(btn.is_active());
        });
    }
    {
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let overview_forced_hidden = overview_forced_hidden.clone();
        let state = state.clone();
        overview_pane_toggle.connect_toggled(move |btn| {
            state.borrow().settings.set_bool(crate::settings::LAYOUT_CALENDAR_OVERVIEW, btn.is_active());
            // The window-width check may have auto-hidden the pane while the
            // window is too narrow; don't override that here.
            mail_calendar_overview_card.set_visible(btn.is_active() && !overview_forced_hidden.get());
        });
    }
    {
        // Conversations (View ▸ Messages, Config → Appearance): re-render the
        // message list in or out of thread grouping. The mode lives in the
        // model (`set_threaded`), and the setting persists it - mirroring how
        // the sort controls own their state in `UiState` while GSettings
        // mirrors it. `suppress_rows` stops the Config rows' own notify
        // handlers from echoing a change back through here.
        let suppress_rows: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let apply_threaded: Rc<dyn Fn(bool, bool)> = {
            let state = state.clone();
            let message_list = message_list.clone();
            let config_view = config_view.clone();
            let message_organization_action = message_organization_action.clone();
            let conversation_order_action = conversation_order_action.clone();
            let suppress_rows = suppress_rows.clone();
            Rc::new(move |threaded: bool, persist: bool| {
                message_organization_action.set_state(&message_organization_value(threaded).to_variant());
                conversation_order_action.set_enabled(threaded);
                suppress_rows.set(true);
                config_view.conversations_row.set_active(threaded);
                suppress_rows.set(false);
                if persist {
                    state.borrow().settings.set_bool(crate::settings::MAIL_THREADED, threaded);
                }
                message_list.set_threaded(threaded);
            })
        };
        let apply_order: ApplyConversationOrder = {
            let state = state.clone();
            let config_view = config_view.clone();
            let conversation_order_action = conversation_order_action.clone();
            let suppress_rows = suppress_rows.clone();
            let conversation_context = conversation_context.clone();
            Rc::new(move |choice: &str, persist: bool| {
                let index = crate::settings::conversation_order_index(choice);
                let choice = crate::settings::conversation_order_at(index);
                conversation_order_action.set_state(&choice.to_variant());
                suppress_rows.set(true);
                config_view.conversation_order_row.set_selected(index);
                suppress_rows.set(false);
                if persist {
                    state.borrow().settings.set_string(crate::settings::MAIL_CONVERSATION_ORDER, choice);
                    relayout_conversation(&conversation_context);
                }
            })
        };
        {
            let apply_threaded = apply_threaded.clone();
            message_organization_action.connect_activate(move |_action, parameter| {
                let Some(choice) = parameter.and_then(|p| p.str()) else { return };
                apply_threaded(choice == "conversations", true);
            });
        }
        {
            let apply_threaded = apply_threaded.clone();
            let suppress_rows = suppress_rows.clone();
            config_view.conversations_row.connect_active_notify(move |row| {
                if !suppress_rows.get() {
                    apply_threaded(row.is_active(), true);
                }
            });
        }
        {
            let apply_order = apply_order.clone();
            conversation_order_action.connect_activate(move |_action, parameter| {
                let Some(choice) = parameter.and_then(|p| p.str()) else { return };
                apply_order(choice, true);
            });
        }
        {
            let apply_order = apply_order.clone();
            let suppress_rows = suppress_rows.clone();
            config_view.conversation_order_row.connect_selected_notify(move |row| {
                if !suppress_rows.get() {
                    apply_order(crate::settings::conversation_order_at(row.selected()), true);
                }
            });
        }
        {
            let state = state.clone();
            let conversation_context = conversation_context.clone();
            config_view.conversation_include_sent_row.connect_active_notify(move |row| {
                let store = state.borrow().settings.clone();
                if store.get_bool(crate::settings::MAIL_CONVERSATION_INCLUDE_SENT) != row.is_active() {
                    store.set_bool(crate::settings::MAIL_CONVERSATION_INCLUDE_SENT, row.is_active());
                    relayout_conversation(&conversation_context);
                }
            });
        }
        // Seed every control from the stored state without writing back.
        let persisted = state.borrow().settings.clone();
        apply_threaded(persisted.get_bool(crate::settings::MAIL_THREADED), false);
        apply_order(&persisted.get_string(crate::settings::MAIL_CONVERSATION_ORDER), false);
        config_view
            .conversation_include_sent_row
            .set_active(persisted.get_bool(crate::settings::MAIL_CONVERSATION_INCLUDE_SENT));
    }
    // Phase 5: apply the persisted Layout toggles now that their handlers are
    // wired (each `set_active` fires `toggled`, which sets the card's
    // visibility), so the pane layout comes back as the user left it. The
    // handlers' write-through makes these calls no-ops on restart.
    {
        let persisted = state.borrow().settings.clone();
        folder_pane_toggle.set_active(persisted.get_bool(crate::settings::LAYOUT_FOLDER_PANE));
        overview_pane_toggle.set_active(persisted.get_bool(crate::settings::LAYOUT_CALENDAR_OVERVIEW));
    }

    // Which sub-page each view should show when its nav-rail button becomes
    // active - kept up to date by the discovery/event handlers below (which
    // only actually flip `root_stack`'s visible child if their own button is
    // the one currently active, so a background sync on the other view
    // never yanks the screen out from under whichever one the user is
    // looking at).
    let current_mail_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("empty"));
    let current_calendar_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("calendar-empty"));
    let current_contacts_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("contacts-empty"));
    let current_tasks_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("tasks-empty"));
    let current_lookout_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("lookout-empty"));
    // The standalone People window, when the People screen is popped out of
    // the main window (see the "Open in new window" contacts-toolbar button).
    // `None` while the paned lives in `root_stack`; `Some` while it lives in
    // its own window, with a placeholder occupying the stack's "contacts"
    // slot. The window is created on demand - never at startup - and closed
    // by re-attaching the paned to the main window. The placeholder starts
    // unparented: it only ever enters `root_stack` on detach (under the
    // "contacts" name) and leaves it again on re-attach, so it can never be
    // added while already parented (which GTK asserts against and which
    // otherwise leaves a duplicate "contacts" child shadowing the paned).
    let contacts_window: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
    let contacts_detached_page = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("People are in a separate window")
        .description("Close that window to bring them back here.")
        .build();
    {
        let root_stack = root_stack.clone();
        let current_mail_page = current_mail_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        let overview_pane_toggle = overview_pane_toggle.clone();
        let overview_forced_hidden = overview_forced_hidden.clone();
        let ribbon_tabs = ribbon_tabs.clone();
        let state = state.clone();
        let search_entry = search_entry.clone();
        let scope_stack = scope_stack.clone();
        let search_refine_button = search_refine_button.clone();
        let calendar_main = calendar_main.clone();
        mail_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("mail");
                exit_calendar_search_if_active(&state, &calendar_main, &search_entry);
                scope_stack.set_visible_child_name("mail");
                search_refine_button.set_visible(true);
                search_entry.set_placeholder_text(Some("Search mail"));
                search_entry.set_tooltip_text(Some("Search mail (Ctrl+F)"));
                root_stack.set_visible_child_name(current_mail_page.get());
                // Keep the contextual Search row on screen when a search is
                // running (a search started from another module activates this
                // button, which must not clobber the row).
                let child = if state.borrow().search_active {
                    "search".to_string()
                } else {
                    ribbon_stack_name("mail", active_ribbon_tab.get()).to_string()
                };
                view_toolbar_stack.set_visible_child_name(&child);
                // Respect the View tab's toggle rather than forcing the
                // overview pane back on after a Calendar round-trip -
                // and the width-based auto-hide while the window is narrow.
                mail_calendar_overview_card.set_visible(overview_pane_toggle.is_active() && !overview_forced_hidden.get());
                ribbon_tabs.set_sensitive(true);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_calendar_page = current_calendar_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let ribbon_tabs = ribbon_tabs.clone();
        let search_entry = search_entry.clone();
        let scope_stack = scope_stack.clone();
        let search_refine_button = search_refine_button.clone();
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        calendar_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("calendar");
                // Search is module-scoped: a mail search showing its results
                // is left behind before Calendar takes over the title bar.
                if state.borrow().search_active {
                    exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, &search_entry);
                } else if !search_entry.text().is_empty() {
                    // A mail query still waiting out its debounce must not
                    // leak into the Calendar placeholder.
                    search_entry.set_text("");
                }
                scope_stack.set_visible_child_name("calendar");
                search_refine_button.set_visible(false);
                search_entry.set_placeholder_text(Some("Search calendar"));
                search_entry.set_tooltip_text(Some("Search calendar (Ctrl+F)"));
                root_stack.set_visible_child_name(current_calendar_page.get());
                view_toolbar_stack.set_visible_child_name("calendar");
                mail_calendar_overview_card.set_visible(false);
                ribbon_tabs.set_sensitive(false);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_contacts_page = current_contacts_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let ribbon_tabs = ribbon_tabs.clone();
        let contacts_window = contacts_window.clone();
        let search_entry = search_entry.clone();
        let scope_stack = scope_stack.clone();
        let search_refine_button = search_refine_button.clone();
        let state = state.clone();
        let calendar_main = calendar_main.clone();
        let worker = worker.clone();
        contacts_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("contacts");
                // Re-read the desktop's address books (E1.5), so a contact
                // added in GNOME Contacts shows without waiting for the timer.
                crate::desktop_contacts::reload(&state, &worker);
                exit_calendar_search_if_active(&state, &calendar_main, &search_entry);
                scope_stack.set_visible_child_name("mail");
                search_refine_button.set_visible(true);
                search_entry.set_placeholder_text(Some("Search contacts"));
                search_entry.set_tooltip_text(Some("Search contacts (Ctrl+F)"));
                root_stack.set_visible_child_name(current_contacts_page.get());
                view_toolbar_stack.set_visible_child_name("contacts");
                mail_calendar_overview_card.set_visible(false);
                ribbon_tabs.set_sensitive(false);
                // People may have been popped out into their own window -
                // bring that to the front instead of the in-stack placeholder.
                if let Some(win) = contacts_window.borrow().as_ref() {
                    win.present();
                }
            }
        });
    }
    // The People screen's Desktop section (E1.5) also refreshes on a timer
    // while Lookout runs, at the CardDAV poll's cadence.
    {
        let state = state.clone();
        let worker = worker.clone();
        glib::timeout_add_local(crate::desktop_contacts::RELOAD_INTERVAL, move || {
            crate::desktop_contacts::reload(&state, &worker);
            glib::ControlFlow::Continue
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_tasks_page = current_tasks_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let ribbon_tabs = ribbon_tabs.clone();
        let search_entry = search_entry.clone();
        let scope_stack = scope_stack.clone();
        let search_refine_button = search_refine_button.clone();
        let state = state.clone();
        let calendar_main = calendar_main.clone();
        tasks_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("tasks");
                exit_calendar_search_if_active(&state, &calendar_main, &search_entry);
                scope_stack.set_visible_child_name("mail");
                search_refine_button.set_visible(true);
                search_entry.set_placeholder_text(Some("Search mail"));
                search_entry.set_tooltip_text(Some("Search mail (Ctrl+F)"));
                root_stack.set_visible_child_name(current_tasks_page.get());
                view_toolbar_stack.set_visible_child_name("tasks");
                mail_calendar_overview_card.set_visible(false);
                ribbon_tabs.set_sensitive(false);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_lookout_page = current_lookout_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let ribbon_tabs = ribbon_tabs.clone();
        let search_entry = search_entry.clone();
        let scope_stack = scope_stack.clone();
        let search_refine_button = search_refine_button.clone();
        let state = state.clone();
        let calendar_main = calendar_main.clone();
        lookout_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("lookout");
                exit_calendar_search_if_active(&state, &calendar_main, &search_entry);
                scope_stack.set_visible_child_name("mail");
                search_refine_button.set_visible(true);
                search_entry.set_placeholder_text(Some("Search mail"));
                search_entry.set_tooltip_text(Some("Search mail (Ctrl+F)"));
                root_stack.set_visible_child_name(current_lookout_page.get());
                view_toolbar_stack.set_visible_child_name("lookout");
                mail_calendar_overview_card.set_visible(false);
                ribbon_tabs.set_sensitive(false);
            }
        });
    }

    // --- Pop-out People window: "Open in new window" moves the contacts
    // paned out of `root_stack` into a standalone `adw::Window` (the app's
    // first multi-window surface); closing that window re-attaches the paned
    // to the main window, preserving every widget's state. The window gets
    // its own header bar (title "People") so it can be dragged, with a "Back
    // to main window" button that closes it - and the standard close button
    // does the same, both via the `close-request` handler below. While
    // detached, a placeholder occupies the stack's "contacts" slot so the
    // page name stays valid for every other code path (rail handler,
    // discovery `show_page`). Only one instance can exist; re-clicking the
    // button just re-presents it. ---
    {
        let contacts_paned = contacts_paned.clone();
        let root_stack = root_stack.clone();
        let contacts_detached_page = contacts_detached_page.clone();
        let current_module = current_module.clone();
        let current_contacts_page = current_contacts_page.clone();
        let contacts_view_button = contacts_view_button.clone();
        let contacts_window = contacts_window.clone();
        open_contacts_window_button.connect_clicked(move |_| {
            if let Some(existing) = contacts_window.borrow().as_ref() {
                existing.present();
                return;
            }
            let win = adw::Window::builder().default_width(960).default_height(680).build();
            let header = adw::HeaderBar::new();
            let people_title = adw::WindowTitle::new("People", "");
            crate::privacy::mark_ui(&people_title);
            header.set_title_widget(Some(&people_title));
            let back_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popin1", "go-previous-symbolic", "go-back-symbolic"]));
            back_button.set_tooltip_text(Some("Return the People screen to the main window"));
            header.pack_start(&back_button);
            {
                let win = win.clone();
                back_button.connect_clicked(move |_| win.close());
            }
            let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
            content_box.append(&header);
            {
                let contacts_paned = contacts_paned.clone();
                let root_stack = root_stack.clone();
                let contacts_detached_page = contacts_detached_page.clone();
                let current_module = current_module.clone();
                let current_contacts_page = current_contacts_page.clone();
                let contacts_view_button = contacts_view_button.clone();
                let contacts_window = contacts_window.clone();
                win.connect_close_request(move |_| {
                    // Move the paned back into the main window's stack. The
                    // placeholder is removed by widget, not by name - it was
                    // added under "contacts" on detach so the page name
                    // stays valid while detached, and it must not linger as
                    // a second "contacts" child shadowing the paned.
                    if let Some(parent) = contacts_paned.parent().and_then(|parent| parent.downcast::<gtk::Box>().ok()) {
                        parent.remove(&contacts_paned);
                    }
                    root_stack.remove(&contacts_detached_page);
                    root_stack.add_named(&contacts_paned, Some("contacts"));
                    if current_module.get() == "contacts" && contacts_view_button.is_active() {
                        root_stack.set_visible_child_name(current_contacts_page.get());
                    }
                    *contacts_window.borrow_mut() = None;
                    glib::Propagation::Proceed
                });
            }
            // Move the paned out of the main stack into the new window, and
            // put the placeholder into its old slot. The placeholder must be
            // unparented first (it never lives in the stack while the paned
            // is attached) - `add_named` on an already-parented widget trips
            // GTK's `gtk_widget_set_parent` assertion.
            root_stack.remove(&contacts_paned);
            root_stack.remove(&contacts_detached_page);
            root_stack.add_named(&contacts_detached_page, Some("contacts"));
            if current_module.get() == "contacts" {
                root_stack.set_visible_child_name("contacts");
            }
            content_box.append(&contacts_paned);
            contacts_paned.set_vexpand(true);
            win.set_content(Some(&content_box));
            *contacts_window.borrow_mut() = Some(win.clone());
            win.present();
        });
    }

    // --- Full-text search wiring: the entry lives permanently in the header
    // bar, and a query started from *any* module switches to Mail (activating
    // its nav button re-runs the module handler, which re-shows the mail
    // page and re-enables Home/View) so the results are visible. The entry
    // itself is built above, next to the state it searches. ---
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        let search_debounce = search_debounce.clone();
        let mail_view_button = mail_view_button.clone();
        let current_module = current_module.clone();
        let calendar_main = calendar_main.clone();
        search_entry.connect_search_changed(move |entry| {
            let query = entry.text();
            // Calendar event search (CAL-22 / UX-12): the title bar follows
            // the active module, so a query typed here searches cached events
            // and shows the results page instead of switching to Mail.
            if current_module.get() == "calendar" {
                search_debounce.set(search_debounce.get() + 1);
                if query.trim().is_empty() {
                    if let Some(calendar_state) = active_calendar_state(&state) {
                        exit_calendar_search(&calendar_state, &calendar_main, entry);
                    }
                    return;
                }
                let token = search_debounce.get();
                let state = state.clone();
                let calendar_main = calendar_main.clone();
                let worker = worker.clone();
                let search_debounce = search_debounce.clone();
                let current_module = current_module.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS as u64), move || {
                    if search_debounce.get() != token || current_module.get() != "calendar" {
                        return;
                    }
                    if let Some(calendar_state) = active_calendar_state(&state) {
                        start_calendar_search(&calendar_state, &worker, &calendar_main, &query);
                    }
                });
                return;
            }
            // While the People module is active the title-bar search becomes
            // the People search (UX-12, P-06): filter the contacts list in
            // place rather than switching to Mail. An emptied box clears the
            // filter the same way.
            if current_module.get() == "contacts" {
                search_debounce.set(search_debounce.get() + 1);
                state.borrow_mut().contacts_filter = query.to_string();
                let refresh = state.borrow().contacts_list_refresh.clone();
                if let Some(refresh) = refresh {
                    refresh();
                }
                return;
            }
            if query.trim().is_empty() {
                // Clearing the field (its X, or a programmatic `set_text("")`
                // from `exit_search`) ends the search right away - no debounce
                // needed. Bumping the token also invalidates any timeout armed
                // for a query still being typed.
                search_debounce.set(search_debounce.get() + 1);
                exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, entry);
                return;
            }
            let token = search_debounce.get() + 1;
            search_debounce.set(token);
            // Clone everything the timeout needs: the outer closure is `Fn`
            // and fires again on the next keystroke, so the timeout's own
            // `move` closure can't borrow from it.
            let state = state.clone();
            let worker = worker.clone();
            let message_list = message_list.clone();
            let list_header = list_header.clone();
            let search_debounce = search_debounce.clone();
            let mail_view_button = mail_view_button.clone();
            let current_module = current_module.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS as u64), move || {
                if search_debounce.get() != token || current_module.get() == "calendar" {
                    return;
                }
                start_search(&state, &worker, &message_list, &list_header, query.as_str());
                // Land the results on screen: if the user started typing from
                // another module, activate Mail (no-op if already active).
                mail_view_button.set_active(true);
            });
        });
    }
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        let search_debounce = search_debounce.clone();
        let current_module = current_module.clone();
        let calendar_main = calendar_main.clone();
        // Esc in the entry: leave search mode entirely rather than just
        // clearing the field (`GtkSearchEntry` emits `stop-search` on Esc).
        search_entry.connect_stop_search(move |entry| {
            if search_debounce.get() != 0 {
                // Bump the debounce token so a timeout armed for the query
                // being typed is invalidated before we clear the entry - the
                // `set_text("")` below would otherwise fire `search-changed`
                // and re-enter a search for the (already-cleared) query.
                search_debounce.set(search_debounce.get() + 1);
            }
            if current_module.get() == "calendar" {
                if let Some(calendar_state) = active_calendar_state(&state) {
                    exit_calendar_search(&calendar_state, &calendar_main, entry);
                }
            } else {
                exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, entry);
            }
        });
    }

    // Calendar's ribbon Filter action focuses the title-bar search (CAL-22).
    {
        let search_entry = search_entry.clone();
        filter_button.connect_clicked(move |_| {
            search_entry.grab_focus();
        });
    }

    // --- Search scope, Refine form and history (roadmap 2.3): the scope
    // dropdowns (header and Search ribbon) stay in sync, the Refine buttons
    // open the advanced-search form, and focusing the empty entry offers the
    // recent-search history. Any of these rewrites the entry's text, which
    // re-runs the search through `search-changed`. ---
    {
        let rerun: Rc<dyn Fn(&str)> = {
            let state = state.clone();
            let worker = worker.clone();
            let message_list = message_list.clone();
            let list_header = list_header.clone();
            Rc::new(move |query: &str| {
                if !query.trim().is_empty() {
                    start_search(&state, &worker, &message_list, &list_header, query);
                }
            })
        };
        let on_scope: Rc<dyn Fn(u32)> = {
            let state = state.clone();
            let entry = search_entry.clone();
            let header_dropdown = scope_dropdown.clone();
            let ribbon_dropdown = ribbon_scope_dropdown.clone();
            let rerun = rerun.clone();
            Rc::new(move |index: u32| {
                let scope = search_scope_at(index);
                {
                    let mut st = state.borrow_mut();
                    if st.search_scope == scope {
                        return;
                    }
                    st.search_scope = scope;
                    st.settings.set_string(crate::settings::MAIL_SEARCH_SCOPE, scope.as_str());
                }
                header_dropdown.set_selected(index);
                ribbon_dropdown.set_selected(index);
                let active = state.borrow().search_active;
                if active {
                    rerun(&entry.text());
                }
            })
        };
        scope_dropdown.connect_selected_notify({
            let on_scope = on_scope.clone();
            move |dropdown| on_scope(dropdown.selected())
        });
        ribbon_scope_dropdown.connect_selected_notify({
            let on_scope = on_scope.clone();
            move |dropdown| on_scope(dropdown.selected())
        });

        // Calendar search scope (CAL-22): all connected calendars vs only the
        // ones ticked in the sidebar. Persisted like the mail scope, and an
        // active search re-runs immediately.
        {
            let state = state.clone();
            let worker = worker.clone();
            let calendar_main = calendar_main.clone();
            calendar_scope_dropdown.connect_selected_notify(move |dropdown| {
                let checked = dropdown.selected() == 1;
                let Some(calendar_state) = active_calendar_state(&state) else {
                    return;
                };
                {
                    let mut st = calendar_state.borrow_mut();
                    if st.calendar_search_checked_only == checked {
                        return;
                    }
                    st.calendar_search_checked_only = checked;
                }
                state
                    .borrow()
                    .settings
                    .set_string(crate::settings::CALENDAR_SEARCH_SCOPE, if checked { "checked" } else { "all" });
                if calendar_state.borrow().calendar_search_active {
                    rerun_calendar_search(&calendar_state, &worker, &calendar_main);
                }
            });
        }
        {
            let state = state.clone();
            let clear_calendar_main = calendar_main.clone();
            let entry = search_entry.clone();
            calendar_main.search_clear_button.connect_clicked(move |_| {
                if let Some(calendar_state) = active_calendar_state(&state) {
                    exit_calendar_search(&calendar_state, &clear_calendar_main, &entry);
                }
            });
        }

        let header_refine = build_refine_popover(&state);
        search_refine_button.set_popover(Some(&header_refine));
        let ribbon_refine = build_refine_popover(&state);
        ribbon_refine_button.set_popover(Some(&ribbon_refine));

        {
            let entry = search_entry.clone();
            search_clear_button.connect_clicked(move |_| entry.set_text(""));
        }

        // The history popover, anchored to the entry and shown on focus while
        // the entry is empty (Outlook's recent-search list).
        let history_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let history_popover = gtk::Popover::new();
        history_popover.set_child(Some(&history_box));
        history_popover.set_parent(&search_entry);
        let refresh_history: Rc<dyn Fn()> = {
            let state = state.clone();
            let entry = search_entry.clone();
            let history_box = history_box.clone();
            let popover = history_popover.clone();
            Rc::new(move || {
                while let Some(child) = history_box.first_child() {
                    history_box.remove(&child);
                }
                let recent = state.borrow().recent_searches.clone();
                if recent.is_empty() {
                    let empty = gtk::Label::builder().label("No recent searches").xalign(0.0).css_classes(["dim-label"]).build();
                    crate::privacy::mark_ui(&empty);
                    history_box.append(&empty);
                    return;
                }
                for query in recent {
                    let button = gtk::Button::with_label(&query);
                    // A previous search is content: blurred by privacy mode.
                    crate::privacy::mark_content(&button);
                    button.add_css_class("flat");
                    button.set_halign(gtk::Align::Start);
                    {
                        let entry = entry.clone();
                        let popover = popover.clone();
                        button.connect_clicked(move |_| {
                            entry.set_text(&query);
                            popover.popdown();
                        });
                    }
                    history_box.append(&button);
                }
            })
        };
        let focus = gtk::EventControllerFocus::new();
        {
            let popover = history_popover.clone();
            let entry = search_entry.clone();
            let refresh_history = refresh_history.clone();
            focus.connect_enter(move |_| {
                if entry.text().is_empty() {
                    refresh_history();
                    popover.popup();
                }
            });
        }
        search_entry.add_controller(focus);
    }

    // The nav rail runs the full height *below the title bar* - it sits
    // beside the menu bar/command toolbar/mail-or-calendar content, but not
    // beside `window_header` itself, which stays the one real title bar
    // spanning the full window width at the very top.
    root_stack.set_hexpand(true);
    root_stack.set_vexpand(true);

    // The icon command toolbar (Mail's/Calendar's) gets its own dark grey
    // subgroup, distinct from the menu bar's black background.
    //
    // Its right end carries the "Ribbon layout" menu - the Simplified/Classic
    // switch, Outlook's own layout chevron. It sits outside the module stack
    // so it is present in every module and on every tab; `win.ribbon-style`
    // is the stateful action behind it (see the ribbon wiring above).
    let ribbon_layout_menu = gio::Menu::new();
    {
        let section = gio::Menu::new();
        for mode in crate::ribbon::RibbonMode::ALL {
            section.append(Some(mode.label()), Some(&format!("win.ribbon-style('{}')", mode.code())));
        }
        ribbon_layout_menu.append_section(None, &section);
    }
    let ribbon_layout_button = gtk::MenuButton::builder()
        .tooltip_text("Ribbon layout")
        .valign(gtk::Align::Center)
        .menu_model(&ribbon_layout_menu)
        .build();
    // Dress it as a ribbon control in a one-button band of its own, so it
    // carries the same `.ribbon-control`/`.flat` classes and the same
    // Simplified/Classic geometry as every module's controls instead of
    // sitting at Adwaita's default size beside them. It is shrinkable: if
    // even the icon-only ribbon page does not fit, Layout is the first
    // control moved into More.
    let ribbon_chrome_band = crate::ribbon::RibbonBand::new(&ribbon_style, &[]);
    ribbon_chrome_band.standalone().menu(&ribbon_layout_button, "view-grid-symbolic", "Layout");
    let ribbon_chrome = ribbon_chrome_band.root().clone();

    // The trailing More button is a band of its own and must remain visible
    // at any width, so its natural width is always reserved by the overflow
    // fit (it is never wrapped in the shrinkable scroller below).
    let ribbon_more_band = crate::ribbon::RibbonBand::new(&ribbon_style, &[]);
    ribbon_more_band.standalone().menu(&more_button, "view-more-symbolic", "More");
    let ribbon_more = ribbon_more_band.root().clone();

    let icon_toolbar_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["window-icon-toolbar-background"])
        .overflow(gtk::Overflow::Hidden)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    // The ribbon pages have a large natural width, and a plain box would
    // make that the window's minimum width - leaving the ribbon no room to
    // overflow into More. A scrolled window with external scrollbars has a
    // zero minimum while still allocating the page its current width; the
    // overflow controller then hides/moves what no longer fits, so no
    // scrollbar is ever shown or needed.
    let ribbon_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .has_frame(false)
        .kinetic_scrolling(false)
        .hexpand(true)
        .build();
    view_toolbar_stack.set_hexpand(true);
    ribbon_scroller.set_child(Some(&view_toolbar_stack));
    icon_toolbar_box.append(&ribbon_scroller);
    icon_toolbar_box.append(&ribbon_chrome);
    icon_toolbar_box.append(&ribbon_more);

    // The adaptive overflow controller: it measures the active ribbon page,
    // drops labels when space is tight, then moves the controls that still
    // do not fit (Layout first) into the More popover.
    let ribbon_overflow = crate::ribbon::RibbonOverflow::new(&ribbon_style, &more_button, &more_overflow_box, &more_extras_box, &more_separator, &ribbon_more_band);
    ribbon_overflow.add_band(&command_band);
    ribbon_overflow.add_band(&send_receive_band);
    ribbon_overflow.add_band(&folder_band);
    ribbon_overflow.add_band(&view_band);
    ribbon_overflow.add_band(&help_band);
    ribbon_overflow.add_band(&calendar_band);
    ribbon_overflow.add_band(&tasks_band);
    ribbon_overflow.add_band(&contacts_band);
    ribbon_overflow.add_band(&lookout_band);
    ribbon_overflow.add_band(&search_band);
    ribbon_overflow.set_layout_band(&ribbon_chrome_band);
    ribbon_overflow.set_mail_home_band(&command_band);
    ribbon_overflow.attach(&icon_toolbar_box, &view_toolbar_stack);

    // Rebuild the Mail Home extras (if that is the page showing) each time
    // More opens. The overflowed controls live in the same popover and are
    // maintained by the controller, so they are not disturbed here.
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker_for_menu = worker.clone();
        let reading_stack_for_menu = reading_stack.clone();
        let more_extras_box = more_extras_box.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let command_toolbar = command_toolbar.clone();
        let ribbon_overflow = ribbon_overflow.clone();
        more_popover.connect_show(move |popover| {
            while let Some(child) = more_extras_box.first_child() {
                more_extras_box.remove(&child);
            }
            let mail_home_showing = view_toolbar_stack
                .visible_child()
                .is_some_and(|visible| visible == command_toolbar.clone().upcast::<gtk::Widget>());
            more_extras_box.set_visible(mail_home_showing);
            if mail_home_showing {
                let has_selection = message_list.selected_summary().is_some();
                let menu = build_more_menu(has_selection, &message_list, &state, &worker_for_menu, &reading_stack_for_menu, popover);
                more_extras_box.append(&menu);
            }
            ribbon_overflow.sync_sections();
        });
    }

    // Menu bar + icon toolbar grouped onto their own shared black
    // background, rather than each row painting (or not painting) its own.
    // Rounded like the icon toolbar's grey subgroup, and margined so the
    // corners show against the window background image.
    let toolbars_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["window-toolbars-background"])
        .overflow(gtk::Overflow::Hidden)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    toolbars_box.append(&menu_bar);
    toolbars_box.append(&icon_toolbar_box);

    let inner_content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).vexpand(true).build();
    inner_content_box.append(&toolbars_box);
    inner_content_box.append(&root_stack);

    // Resizable split between the main content and the overview pane -
    // `nav_rail` stays a fixed-width sibling outside the split (it isn't
    // meant to be resizable), but the overview pane's width is user-
    // draggable like every other pane split in this app.
    let content_and_overview_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&inner_content_box)
        .end_child(&mail_calendar_overview_card)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(1340)
        .build();
    content_and_overview_paned.add_css_class("seamless-paned");

    // If the window gets too narrow to fit the overview pane's whole minimum
    // width beside the content's minimum, GTK clips the pane's right edge
    // instead of shrinking it (both paned children refuse to shrink). Hide
    // the pane in that case, and bring it back - if the View tab's toggle is
    // still on - once a resize widens the window enough again. The separator
    // also takes space between the children, so the fit threshold includes a
    // small cushion so the pane neither clips nor flickers exactly at the
    // boundary. `measure` ignores visibility, so the check works whether the
    // pane is currently shown or hidden.
    let check_overview_fits = {
        let content_and_overview_paned = content_and_overview_paned.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let overview_pane_toggle = overview_pane_toggle.clone();
        let overview_forced_hidden = overview_forced_hidden.clone();
        let current_module = current_module.clone();
        move || {
            // Not allocated yet (build time / before the first map): leave
            // visibility alone until the first real resize check.
            if content_and_overview_paned.width() <= 0 {
                return;
            }
            let start_min = content_and_overview_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
            let end_min = mail_calendar_overview_card.measure(gtk::Orientation::Horizontal, -1).0;
            let fits = content_and_overview_paned.width() >= start_min + end_min + 8;
            if !fits {
                overview_forced_hidden.set(true);
                mail_calendar_overview_card.set_visible(false);
            } else if overview_forced_hidden.replace(false) {
                // Only the Mail module shows the overview pane; the other
                // modules' handlers own hiding it there.
                if current_module.get() == "mail" {
                    mail_calendar_overview_card.set_visible(overview_pane_toggle.is_active());
                }
            }
        }
    };

    let window_body = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
    window_body.append(&nav_rail);
    window_body.append(&content_and_overview_paned);

    let outer_toolbar_view = adw::ToolbarView::new();
    outer_toolbar_view.add_top_bar(&window_header);
    outer_toolbar_view.set_content(Some(&window_body));

    toast_overlay.set_child(Some(&outer_toolbar_view));
    toast_overlay.set_hexpand(true);
    toast_overlay.set_vexpand(true);

    let window_overlay = gtk::Overlay::new();
    window_overlay.set_child(Some(&background));
    // The dimming layer between the background image and the app content:
    // an opaque-black widget whose opacity (1 - brightness) darkens a
    // user-picked background toward black per Config → Appearance →
    // "Background dimming". `can_target(false)` keeps it click-through.
    // Starts at 50% opacity - the bundled artwork's default brightness. A
    // custom background overrides this below with its own stored GSettings
    // value once we know one is in use.
    let background_dim = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    background_dim.set_can_target(false);
    background_dim.set_css_classes(&["window-background-dim"]);
    background_dim.set_opacity(0.75);
    window_overlay.add_overlay(&background_dim);
    window_overlay.add_overlay(&toast_overlay);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lookout")
        .default_width(settings.get_int(crate::settings::WINDOW_WIDTH))
        .default_height(settings.get_int(crate::settings::WINDOW_HEIGHT))
        .content(&window_overlay)
        .build();
    // Restored once here, at construction - the window object then lives for
    // the app's lifetime, so this never needs to run again (tray show/hide
    // just toggles visibility on the same window).
    if settings.get_bool(crate::settings::WINDOW_MAXIMIZED) {
        window.maximize();
    }
    // The message list's sort menu is built long before the window exists;
    // `win.`-scoped actions resolve through the widget hierarchy when the menu
    // item is activated, so registering it here is in time.
    window.add_action(&sort_key_action);
    for action in &filter_actions {
        window.add_action(action);
    }
    window.add_action(&reading_pane_action);
    window.add_action(&density_action);
    window.add_action(&message_organization_action);
    window.add_action(&conversation_order_action);
    window.add_action(&task_sort_action);
    window.add_action(&task_group_action);
    window.add_action(&task_show_completed_action);
    window.add_action(&ribbon_style_action);
    window.add_action(&icon_style_action);

    // --- Close-to-background: the window's close button hides the window
    // instead of exiting (Config → General → "Keep running when the window
    // is closed"), so account sync and the notification loops keep running;
    // File → Quit (`app.quit`) is the real exit. The portal status line
    // (v2 `SetStatus`) tells the shell's background-apps list what the
    // hidden app is doing; clearing it on show keeps that list honest.
    {
        let settings = settings.clone();
        let state = state.clone();
        let folder_selection = folder_selection.clone();
        // Flush the folder pane's expansion state: the user's last chevron
        // toggle might not have been followed by a rebuild (the other
        // write-back point), so fold the live model into the session memory
        // and write it out. Shared by the two flush points below (window
        // close and app quit). The merge is a no-op when the pane has no
        // model, so a flush before the first build can't wipe anything.
        let flush_folder_expansion = {
            let state = state.clone();
            let folder_selection = folder_selection.clone();
            move || {
                merge_folder_expansion(&state, &folder_selection);
                persist_folder_expansion(&state);
            }
        };
        window.connect_close_request({
            let worker = worker.clone();
            let flush_folder_expansion = flush_folder_expansion.clone();
            move |win| {
                // Flush geometry synchronously rather than relying on the
                // debounced resize/maximize saves below, in case the window
                // closes right as a resize is still settling.
                if !win.is_maximized() {
                    settings.set_int(crate::settings::WINDOW_WIDTH, win.width());
                    settings.set_int(crate::settings::WINDOW_HEIGHT, win.height());
                }
                settings.set_bool(crate::settings::WINDOW_MAXIMIZED, win.is_maximized());
                flush_folder_expansion();
                if !settings.get_bool(crate::settings::CLOSE_TO_BACKGROUND) {
                    return glib::Propagation::Proceed;
                }
                worker.spawn(crate::background::set_background_status("Syncing your mail and calendar"));
                win.set_visible(false);
                glib::Propagation::Stop
            }
        });
        // The real exit paths - Ctrl+Q / File → Quit, and the tray icon's
        // Quit - call `app.quit()` directly, so the close-request handler
        // (and with it the flush above) never runs for them. The `shutdown`
        // signal reaches every quit, and GTK emits it before its default
        // handler closes the windows, so the tree is still alive to capture.
        app.connect_shutdown({
            let flush_folder_expansion = flush_folder_expansion.clone();
            move |_| flush_folder_expansion()
        });
        window.connect_show({
            let worker = worker.clone();
            move |_| {
                worker.spawn(crate::background::set_background_status(""));
            }
        });
    }

    // --- Pane widths: persist and reapply. Each paned's `position` change
    // (a horizontal drag) cancels the pending settle and reschedules it, so
    // 150 ms after the drag stops the pane's width as a percentage of the
    // window width is stored in GSettings.
    // When the window itself is resized, the stored percentages are applied
    // back to the panes (clamped to each pane's min/max widths, which is what
    // the drag itself abides by). ---
    //
    // Two pieces of bookkeeping keep a restore from being mistaken for a drag:
    //
    // `pane_widths_applying` is set while `apply_stored_pane_widths` is driving
    // `set_position`. GTK emits `notify::position` *synchronously* from
    // `set_position`, so without it every restore writes its own - possibly
    // clamped - result straight back over the percentage the user dragged to.
    // It only covers that synchronous re-entry, though: GTK *also* re-clamps a
    // position from its layout pass whenever a paned's allocation changes, and
    // that notify lands long after the flag has been cleared.
    //
    // `panes_restored` is what catches those. Per pane it records the paned's
    // allocated width and the position last put on screen, so a later notify
    // can be classified: an unchanged allocation with a moved position is the
    // user dragging, anything else is GTK re-fitting (see
    // `pane_change_is_a_drag`). An absent entry means the pane has not been
    // restored during this map cycle and is still showing its built-in default,
    // which is equally not a width to persist. Cleared on unmap, because a pane
    // that maps again goes through the same unrestored-then-clamped sequence.
    //
    // The accessor closures are the *only* places the map is borrowed, and each
    // holds its borrow for a single statement: the save handlers run
    // re-entrantly inside `set_position`, so a borrow held across one would be
    // an immediate `BorrowMutError`.
    let pane_widths_applying: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let panes_restored: Rc<RefCell<HashMap<&'static str, PaneRestoreState>>> = Rc::new(RefCell::new(HashMap::new()));
    // Each pane's allocated width as of the previous restore attempt, so an
    // attempt can tell a settled allocation from one still catching up with
    // the window. Cleared on unmap with the restore record, since a remap
    // starts the whole settle sequence again.
    let pane_alloc_seen: Rc<RefCell<HashMap<&'static str, i32>>> = Rc::new(RefCell::new(HashMap::new()));
    let mark_pane_restored = {
        let panes_restored = panes_restored.clone();
        move |key: &'static str, paned_extent: i32, position: i32| {
            panes_restored.borrow_mut().insert(key, PaneRestoreState { paned_extent, position });
        }
    };
    let clear_pane_restored = {
        let panes_restored = panes_restored.clone();
        let pane_alloc_seen = pane_alloc_seen.clone();
        move |key: &'static str| {
            panes_restored.borrow_mut().remove(key);
            pane_alloc_seen.borrow_mut().remove(key);
        }
    };
    let pane_restore_state = {
        let panes_restored = panes_restored.clone();
        move |key: &'static str| panes_restored.borrow().get(key).copied()
    };
    // Returns whether every mapped pane settled: a pane that is mapped but not
    // yet allocated (no width to compute against) reports `false` so the
    // scheduler below can try again on the next tick, which is what makes the
    // restore deterministic instead of dependent on an incidental resize.
    //
    // `stable` says whether `window_width` has been confirmed by two
    // consecutive attempts. Until it has, positions are still applied - it is
    // better for a pane to be approximately right for one frame than to sit at
    // its built-in default - but no pane is *marked* restored, so nothing that
    // happens against a transient width can be persisted. `window.maximize()`
    // runs before `present()`, so the first width a maximized window reports is
    // the stored un-maximized one; persisting a percentage of that is how the
    // remembered widths got corrupted.
    let apply_stored_pane_widths = {
        let main_paned = main_paned.clone();
        let messages_reading_paned = messages_reading_paned.clone();
        let calendar_paned = calendar_paned.clone();
        let contacts_paned = contacts_paned.clone();
        let contacts_content_paned = contacts_content_paned.clone();
        let config_paned = config_view.paned.clone();
        let content_and_overview_paned = content_and_overview_paned.clone();
        let state = state.clone();
        let pane_widths_applying = pane_widths_applying.clone();
        let mark_pane_restored = mark_pane_restored.clone();
        let pane_alloc_seen = pane_alloc_seen.clone();
        let apply_generation: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        // `give_up` is the scheduler's last attempt: a pane that still has no
        // allocation by then is counted as restored anyway, so its saves
        // reopen. Suppressing a real drag forever would be a worse failure
        // than restoring one pane late.
        move |window_width: i32, window_height: i32, stable: bool, give_up: bool| -> bool {
            if window_width <= 0 {
                return false;
            }
            // Nothing below may `return` early: a return that skipped the
            // reset at the bottom would leave the flag stuck on and silently
            // disable every pane-width save for the rest of the session.
            pane_widths_applying.set(true);
            let generation = apply_generation.get().wrapping_add(1);
            apply_generation.set(generation);
            let settings = state.borrow().settings.clone();
            let mut settled = stable;
            {
                // `end_child` marks the one pane (the mail tab's overview) that
                // is its paned's end child, so its stored percentage is its own
                // width rather than the separator's position. `keys` carries a
                // separate percentage per axis, because the message list's paned
                // flips to vertical when the reading pane is at the bottom.
                let mut restore = |paned: &gtk::Paned, keys: PaneExtentKeys, cap: Option<i32>, end_child: bool| {
                    let key = keys.for_paned(paned);
                    if !paned.is_mapped() {
                        tracing::debug!(pane = key, "pane width restore skipped: not mapped");
                        return;
                    }
                    let axis = paned.orientation();
                    let window_extent = match axis {
                        gtk::Orientation::Vertical => window_height,
                        _ => window_width,
                    };
                    let pct = settings.get_double(key);
                    let paned_extent = paned_axis_extent(paned);
                    // A paned's own allocation lags the window's, and by a lot:
                    // a window already reporting its full maximized width can
                    // still hold paneds sized for the pre-maximize one, several
                    // frames later. A position computed against that gets
                    // clamped to a floor that has nothing to do with the final
                    // layout, so - exactly like the window width - an
                    // allocation is only trusted once two attempts agree on it.
                    let previous_alloc = pane_alloc_seen.borrow().get(key).copied();
                    pane_alloc_seen.borrow_mut().insert(key, paned_extent);
                    let trusted = stable && paned_extent > 0 && previous_alloc == Some(paned_extent);
                    let start_min = paned.start_child().map(|w| w.measure(axis, -1).0).unwrap_or(0);
                    let end_min = paned.end_child().map(|w| w.measure(axis, -1).0).unwrap_or(0);
                    let position = if end_child {
                        paned_position_for_end_percent(pct, window_extent, paned_extent, start_min, end_min)
                    } else {
                        paned_position_for_percent(pct, window_extent, paned_extent, start_min, end_min, cap)
                    };
                    tracing::debug!(
                        pane = key,
                        pct,
                        window_extent,
                        paned_extent,
                        start_min,
                        end_min,
                        stable,
                        trusted,
                        give_up,
                        ?position,
                        "pane width restore"
                    );
                    // The position is applied on every attempt, trusted or not:
                    // a pane that looks approximately right for one frame beats
                    // one sitting at its built-in default. What waits for trust
                    // is the *record*, since that is what reopens this pane's
                    // saves - and a record made against a transient width or a
                    // half-settled allocation would let the clamp that follows
                    // be persisted as though the user had dragged it.
                    if let Some(position) = position {
                        // Recorded *before* the move, never after:
                        // `set_position` runs the save handler synchronously
                        // and that handler reads this map.
                        if trusted || give_up {
                            mark_pane_restored(key, paned_extent, position);
                        }
                        paned.set_position(position);
                    } else if pct <= 0.0 && (trusted || give_up) {
                        // Never dragged: nothing stored, so nothing to protect,
                        // and the pane's first drag should save normally.
                        mark_pane_restored(key, paned_extent, paned.position());
                    } else if give_up {
                        // Something to restore but still no allocation to
                        // measure against, and out of attempts. Suppressing a
                        // real drag forever would be worse than giving up.
                        mark_pane_restored(key, paned_extent, paned.position());
                    }
                    if !trusted {
                        settled = false;
                    }
                };
                // The outermost split goes first: setting it changes the width
                // the inner paneds are measured against, so restoring it last
                // would leave every inner position computed against a stale
                // allocation and guarantee a follow-up clamp.
                restore(
                    &content_and_overview_paned,
                    PaneExtentKeys::fixed(crate::settings::PANE_MAIL_OVERVIEW_WIDTH_PCT),
                    None,
                    true,
                );
                restore(
                    &main_paned,
                    PaneExtentKeys::fixed(crate::settings::PANE_FOLDER_WIDTH_PCT),
                    Some(FOLDER_PANE_MAX_WIDTH),
                    false,
                );
                restore(
                    &messages_reading_paned,
                    PaneExtentKeys {
                        horizontal: crate::settings::PANE_MESSAGE_LIST_WIDTH_PCT,
                        vertical: crate::settings::PANE_MESSAGE_LIST_HEIGHT_PCT,
                    },
                    None,
                    false,
                );
                restore(&calendar_paned, PaneExtentKeys::fixed(crate::settings::PANE_CALENDAR_SIDEBAR_WIDTH_PCT), None, false);
                restore(&contacts_paned, PaneExtentKeys::fixed(crate::settings::PANE_CONTACTS_SIDEBAR_WIDTH_PCT), None, false);
                restore(&contacts_content_paned, PaneExtentKeys::fixed(crate::settings::PANE_CONTACTS_DETAILS_WIDTH_PCT), None, true);
                restore(&config_paned, PaneExtentKeys::fixed(crate::settings::PANE_CONFIG_SIDEBAR_WIDTH_PCT), None, false);
            }
            // The flag is cleared from an idle, not here, so it also covers the
            // layout pass these `set_position` calls just queued. GtkPaned
            // re-clamps its position during that pass and notifies from there,
            // and the clamp can land on a paned whose allocation has *not*
            // changed - `paned_position_for_percent` doesn't model the handle's
            // own width, so a position near the end child's floor can come back
            // a few pixels short. The allocation comparison in the save handlers
            // cannot see that one, since nothing it compares has moved.
            //
            // GTK4 lays out at `GDK_PRIORITY_REDRAW`, which is a higher priority
            // than the default idle, so this runs after the layout it is waiting
            // on. The generation guard means overlapping applies don't let an
            // earlier one's idle clear a later one's flag - and because a later
            // apply always sets the flag again, a dropped idle can never leave
            // saves disabled for good.
            let generation_for_idle = apply_generation.clone();
            let applying_for_idle = pane_widths_applying.clone();
            glib::idle_add_local_once(move || {
                if generation_for_idle.get() == generation {
                    applying_for_idle.set(false);
                }
            });
            settled
        }
    };
    // Every trigger that should restore the panes funnels through this one
    // debounce: a first map, a stack page switch, a maximize and a surface
    // resize can all land within a few frames of each other, and only the last
    // one is worth acting on - the positions are only meaningful once the
    // allocation they're computed against has settled. When a pane is mapped
    // but still unallocated, or the window width is still moving, the apply
    // reports back and the timer runs again, rather than leaving the pane at
    // its built-in default until something else happens to resize the window.
    //
    // The window width is the slow one. A maximized window is created at its
    // stored *un-maximized* width and only reaches the real one once the
    // compositor has acted, so a width is only trusted once two consecutive
    // attempts agree on it (`window_width_is_stable`). Restoring - and worse,
    // persisting - against that first transient width is the whole bug.
    let schedule_apply_pane_widths = {
        let apply_stored_pane_widths = apply_stored_pane_widths.clone();
        let window_for_apply = window.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        move || {
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let apply = apply_stored_pane_widths.clone();
            let window = window_for_apply.clone();
            let debounce_for_timeout = debounce.clone();
            let attempts = Cell::new(0u8);
            let previous_width: Cell<Option<i32>> = Cell::new(None);
            let previous_height: Cell<Option<i32>> = Cell::new(None);
            debounce.set(Some(glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
                let last_attempt = attempts.get() + 1 >= PANE_RESTORE_ATTEMPTS;
                let width = window.width();
                let height = window.height();
                // On the final attempt the width is taken as-is: one pane
                // restored against a slightly stale width is a far better
                // failure than a pane left at its default forever. Stability
                // is tracked on both axes, since the message list's paned can
                // be vertical and restored against the window height.
                let stable =
                    (window_width_is_stable(previous_width.replace(Some(width)), width) && window_width_is_stable(previous_height.replace(Some(height)), height)) || last_attempt;
                let settled = apply(width, height, stable, last_attempt);
                attempts.set(attempts.get() + 1);
                if settled || last_attempt {
                    debounce_for_timeout.set(None);
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            })));
        }
    };
    // One wiring for all six panes: a `position` notify that is genuinely the
    // user dragging the separator stores the pane's width as a percentage of
    // the window width, 150 ms after the drag settles.
    //
    // `end_child` marks the mail tab's overview pane, whose stored percentage
    // is its own width rather than the separator's position. `cap` marks the
    // folder pane, the one pane with a hard ceiling.
    {
        let wire_pane_width_save = |paned: &gtk::Paned, keys: PaneExtentKeys, end_child: bool, cap: bool| {
            let window_for_save = window.clone();
            let state_for_save = state.clone();
            let applying = pane_widths_applying.clone();
            let restore_state = pane_restore_state.clone();
            let record_pane_state = mark_pane_restored.clone();
            let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
            paned.connect_notify_local(Some("position"), move |paned, _| {
                // Cap the folder pane's width: the drag itself is only bounded
                // by the paned's natural minimums, so snap any overshoot here.
                // Returning lets the re-entrant notify (from `set_position`)
                // run the settle/save logic with the capped value. This stays
                // ahead of the guards below so the ceiling still holds for a
                // pane GTK has laid out but nothing has restored yet; a
                // restore's own positions are already capped, so it never
                // fires from one.
                if cap && paned.position() > FOLDER_PANE_MAX_WIDTH {
                    paned.set_position(FOLDER_PANE_MAX_WIDTH);
                    return;
                }
                // A restore's own `set_position` lands here synchronously.
                if applying.get() {
                    return;
                }
                // The key follows the paned's current orientation, so the
                // message list's vertical drag saves under the height key.
                let key = keys.for_paned(paned);
                let axis = paned.orientation();
                let paned_extent = paned_axis_extent(paned);
                let position = paned.position();
                let recorded = restore_state(key);
                if !pane_change_is_a_drag(recorded, paned_extent, position) {
                    // Either the pane has not been restored during this map
                    // cycle - it is still showing its built-in default, which
                    // the user never chose - or GTK has re-clamped the position
                    // to fit a new allocation. The clamp case still updates the
                    // record, so the next genuine drag is measured against what
                    // is actually on screen now.
                    if recorded.is_some() {
                        tracing::debug!(pane = key, paned_extent, position, "pane position re-clamped by GTK, not saving");
                        record_pane_state(key, paned_extent, position);
                    }
                    // Bailing before `debounce.take()` leaves a genuine drag's
                    // pending save, and its value, alone.
                    return;
                }
                record_pane_state(key, paned_extent, position);
                if let Some(id) = debounce.take() {
                    id.remove();
                }
                let extent = if end_child { paned_extent.saturating_sub(position) } else { position };
                let window_extent = window_axis_extent(window_for_save.upcast_ref::<gtk::Window>(), axis);
                let state_for_timeout = state_for_save.clone();
                let debounce_for_timeout = debounce.clone();
                debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    debounce_for_timeout.set(None);
                    if let Some(pct) = pane_width_percent(extent, window_extent) {
                        tracing::debug!(pane = key, extent, window_extent, pct, "saving dragged pane extent");
                        state_for_timeout.borrow().settings.set_double(key, pct);
                    }
                })));
            });
        };
        wire_pane_width_save(&main_paned, PaneExtentKeys::fixed(crate::settings::PANE_FOLDER_WIDTH_PCT), false, true);
        wire_pane_width_save(
            &messages_reading_paned,
            PaneExtentKeys {
                horizontal: crate::settings::PANE_MESSAGE_LIST_WIDTH_PCT,
                vertical: crate::settings::PANE_MESSAGE_LIST_HEIGHT_PCT,
            },
            false,
            false,
        );
        wire_pane_width_save(&calendar_paned, PaneExtentKeys::fixed(crate::settings::PANE_CALENDAR_SIDEBAR_WIDTH_PCT), false, false);
        wire_pane_width_save(&contacts_paned, PaneExtentKeys::fixed(crate::settings::PANE_CONTACTS_SIDEBAR_WIDTH_PCT), false, false);
        wire_pane_width_save(
            &contacts_content_paned,
            PaneExtentKeys::fixed(crate::settings::PANE_CONTACTS_DETAILS_WIDTH_PCT),
            true,
            false,
        );
        wire_pane_width_save(&config_view.paned, PaneExtentKeys::fixed(crate::settings::PANE_CONFIG_SIDEBAR_WIDTH_PCT), false, false);
        wire_pane_width_save(
            &content_and_overview_paned,
            PaneExtentKeys::fixed(crate::settings::PANE_MAIL_OVERVIEW_WIDTH_PCT),
            true,
            false,
        );
    }
    // Each pane restores itself the moment it maps. This is what makes the
    // restore deterministic: every branch of `apply_stored_pane_widths` is
    // gated on `is_mapped()`, and the six panes map at six different moments -
    // the mail split when account discovery flips `root_stack` off its empty
    // page, the calendar and people sidebars when their tab is first shown,
    // the Config sidebar when the Settings dialog opens, the overview pane with
    // the window itself. Before this, a pane that wasn't mapped when some
    // incidental resize happened to fire simply kept its built-in default.
    // Unmap forgets the pane was restored: mapping again puts it back through
    // GTK's allocation-time clamp, so the save suppression has to reopen.
    {
        let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
        let clear_pane_restored = clear_pane_restored.clone();
        let wire_pane_width_restore = move |paned: &gtk::Paned, keys: PaneExtentKeys| {
            {
                let schedule = schedule_apply_pane_widths.clone();
                paned.connect_map(move |_| schedule());
            }
            let clear = clear_pane_restored.clone();
            paned.connect_unmap(move |paned| clear(keys.for_paned(paned)));
        };
        wire_pane_width_restore(&main_paned, PaneExtentKeys::fixed(crate::settings::PANE_FOLDER_WIDTH_PCT));
        wire_pane_width_restore(
            &messages_reading_paned,
            PaneExtentKeys {
                horizontal: crate::settings::PANE_MESSAGE_LIST_WIDTH_PCT,
                vertical: crate::settings::PANE_MESSAGE_LIST_HEIGHT_PCT,
            },
        );
        wire_pane_width_restore(&calendar_paned, PaneExtentKeys::fixed(crate::settings::PANE_CALENDAR_SIDEBAR_WIDTH_PCT));
        wire_pane_width_restore(&contacts_paned, PaneExtentKeys::fixed(crate::settings::PANE_CONTACTS_SIDEBAR_WIDTH_PCT));
        wire_pane_width_restore(&contacts_content_paned, PaneExtentKeys::fixed(crate::settings::PANE_CONTACTS_DETAILS_WIDTH_PCT));
        wire_pane_width_restore(&config_view.paned, PaneExtentKeys::fixed(crate::settings::PANE_CONFIG_SIDEBAR_WIDTH_PCT));
        wire_pane_width_restore(&content_and_overview_paned, PaneExtentKeys::fixed(crate::settings::PANE_MAIL_OVERVIEW_WIDTH_PCT));
    }
    // --- Reading pane position: Right / Bottom / Off, shared by the View
    // ribbon's dropdown (the stateful `win.reading-pane-position` action) and
    // the Settings → Appearance ComboRow. Wired here rather than at the
    // widgets' construction because switching position needs the message
    // list's paned and the pane-extent scheduler, both of which only exist by
    // this point. Bottom flips the paned vertical and puts the reading pane
    // back on its remembered height (or a sensible default the first time);
    // Right flips it back to horizontal.
    {
        let reading_pane_action = reading_pane_action.clone();
        let reading_pane_button = reading_pane_button.clone();
        let messages_reading_paned = messages_reading_paned.clone();
        let message_card = message_card.clone();
        let reading_card = reading_card.clone();
        let reading_pane_row = config_view.reading_pane_row.clone();
        let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
        let state = state.clone();
        // Guards the ComboRow's `selected` notify while `apply` writes it, so
        // the row and the action can never drive each other in a loop.
        let suppress_row: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // The margins a bottom reading pane zeroes to close the half-gap to
        // the message list; captured so Right can restore them.
        let reading_card_default_margin_top = reading_card.margin_top();
        let message_card_default_margin_bottom = message_card.margin_bottom();
        let apply: ApplyReadingPane = {
            let reading_pane_action = reading_pane_action.clone();
            let reading_pane_button = reading_pane_button.clone();
            let messages_reading_paned = messages_reading_paned.clone();
            let message_card = message_card.clone();
            let reading_card = reading_card.clone();
            let reading_pane_row = reading_pane_row.clone();
            let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
            let state = state.clone();
            let suppress_row = suppress_row.clone();
            Rc::new(move |choice: &str, persist: bool| {
                reading_pane_action.set_state(&choice.to_variant());
                let store = state.borrow().settings.clone();
                let visible = choice != "off";
                if persist {
                    store.set_bool(crate::settings::LAYOUT_READING_PANE, visible);
                }
                reading_card.set_visible(visible);
                reading_pane_button.set_tooltip_text(Some(&reading_pane_tooltip(&store)));
                suppress_row.set(true);
                reading_pane_row.set_selected(reading_pane_choice_index(choice));
                suppress_row.set(false);
                if !visible {
                    return;
                }
                let position = crate::settings::reading_pane_position(choice);
                if persist {
                    store.set_string(crate::settings::LAYOUT_READING_PANE_POSITION, choice);
                }
                let orientation = match position {
                    crate::settings::ReadingPanePosition::Bottom => gtk::Orientation::Vertical,
                    crate::settings::ReadingPanePosition::Right => gtk::Orientation::Horizontal,
                };
                messages_reading_paned.set_orientation(orientation);
                match position {
                    crate::settings::ReadingPanePosition::Bottom => {
                        reading_card.set_margin_top(0);
                        message_card.set_margin_bottom(0);
                    }
                    crate::settings::ReadingPanePosition::Right => {
                        reading_card.set_margin_top(reading_card_default_margin_top);
                        message_card.set_margin_bottom(message_card_default_margin_bottom);
                    }
                }
                // A first switch to an orientation with no remembered extent
                // gets a sensible default; the scheduler below overrides it
                // when a stored percentage exists (and re-measures the newly
                // vertical paned).
                if messages_reading_paned.is_mapped() && paned_axis_extent(&messages_reading_paned) > 0 {
                    let stored = store.get_double(match orientation {
                        gtk::Orientation::Vertical => crate::settings::PANE_MESSAGE_LIST_HEIGHT_PCT,
                        _ => crate::settings::PANE_MESSAGE_LIST_WIDTH_PCT,
                    });
                    if stored <= 0.0 {
                        let extent = paned_axis_extent(&messages_reading_paned);
                        messages_reading_paned.set_position((extent as f64 * 0.55).round() as i32);
                    }
                }
                schedule_apply_pane_widths();
            })
        };
        {
            let apply = apply.clone();
            reading_pane_action.connect_activate(move |_action, parameter| {
                let Some(choice) = parameter.and_then(|p| p.str()) else { return };
                apply(choice, true);
            });
        }
        {
            let apply = apply.clone();
            let suppress_row = suppress_row.clone();
            config_view.reading_pane_row.connect_selected_notify(move |row| {
                if suppress_row.get() {
                    return;
                }
                if let Some(choice) = READING_PANE_CHOICES.get(row.selected() as usize) {
                    apply(choice, true);
                }
            });
        }
        // Seed both controls from the stored state without writing anything
        // back through.
        let choice = reading_pane_action_state(&state.borrow().settings);
        apply(&choice, false);
    }
    // The ribbon's layout switch: the band's own "Ribbon layout" menu and
    // Config → Appearance's "Ribbon style" row both drive `win.ribbon-style`,
    // and this one handler keeps the mode, the setting and the row in step.
    {
        let ribbon_style_action = ribbon_style_action.clone();
        let state = state.clone();
        let ribbon_style_row = config_view.ribbon_style_row.clone();
        // Guards the ComboRow's `selected` notify while `apply` writes it.
        let suppress_row: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let initial_mode = ribbon_style.mode();
        let apply: Rc<dyn Fn(crate::ribbon::RibbonMode, bool)> = {
            let ribbon_style = ribbon_style.clone();
            let ribbon_style_action = ribbon_style_action.clone();
            let ribbon_style_row = ribbon_style_row.clone();
            let suppress_row = suppress_row.clone();
            Rc::new(move |mode, persist| {
                ribbon_style.set_mode(mode);
                ribbon_style_action.set_state(&mode.code().to_variant());
                if persist {
                    state.borrow().settings.set_string(crate::settings::RIBBON_STYLE, mode.code());
                }
                suppress_row.set(true);
                ribbon_style_row.set_selected(mode.index());
                suppress_row.set(false);
            })
        };
        {
            let apply = apply.clone();
            ribbon_style_action.connect_activate(move |_action, parameter| {
                let mode = parameter.and_then(|p| p.str()).map(crate::ribbon::RibbonMode::from_code).unwrap_or_default();
                apply(mode, true);
            });
        }
        {
            let apply = apply.clone();
            let suppress_row = suppress_row.clone();
            ribbon_style_row.connect_selected_notify(move |row| {
                if suppress_row.get() {
                    return;
                }
                let mode = crate::ribbon::RibbonMode::ALL.get(row.selected() as usize).copied().unwrap_or_default();
                apply(mode, true);
            });
        }
        // Seed the row from the stored mode without writing anything back.
        apply(initial_mode, false);
    }
    // The optional Fluent icon pack (UX-80): View ▸ Icons and Config →
    // Appearance's "Icon style" row both drive `win.icon-style`, and one
    // handler keeps the pack, the setting and the row in step. A failed
    // switch (an unwritable cache, no display) leaves GNOME in force and puts
    // both controls back where they were.
    {
        let icon_style = icon_style.clone();
        let icon_style_action = icon_style_action.clone();
        let icon_style_row = config_view.icon_style_row.clone();
        let state = state.clone();
        let suppress_row: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let initial_style = icon_style.style();
        let apply: Rc<dyn Fn(crate::icon_style::IconStyle, bool)> = {
            let icon_style = icon_style.clone();
            let icon_style_action = icon_style_action.clone();
            let icon_style_row = icon_style_row.clone();
            let state = state.clone();
            let suppress_row = suppress_row.clone();
            let pin_icon = pin_icon.clone();
            let message_list = message_list.clone();
            Rc::new(move |style, persist| {
                let changed = icon_style.style() != style;
                if changed {
                    if let Err(error) = icon_style.set_style(style) {
                        tracing::warn!(%error, "could not switch the icon style; keeping GNOME icons");
                        let actual = icon_style.style();
                        icon_style_action.set_state(&actual.code().to_variant());
                        suppress_row.set(true);
                        icon_style_row.set_selected(actual.index());
                        suppress_row.set(false);
                        if let Some(overlay) = state.borrow().toast_overlay.as_ref() {
                            overlay.add_toast(adw::Toast::new("Fluent icons are unavailable; keeping GNOME icons"));
                        }
                        return;
                    }
                    // Pin is the one glyph GTK cannot recolour in the bundled
                    // artwork (its name is not symbolic), so its ribbon button
                    // takes the pack's symbolic pin in Fluent mode; refresh it
                    // now rather than waiting for the next selection change.
                    refresh_pin_button(&pin_icon, &message_list, &state);
                }
                icon_style_action.set_state(&style.code().to_variant());
                if persist {
                    state.borrow().settings.set_string(crate::settings::ICON_STYLE, style.code());
                }
                suppress_row.set(true);
                icon_style_row.set_selected(style.index());
                suppress_row.set(false);
            })
        };
        {
            let apply = apply.clone();
            icon_style_action.connect_activate(move |_action, parameter| {
                let style = parameter
                    .and_then(|parameter| parameter.str())
                    .map(crate::icon_style::IconStyle::from_code)
                    .unwrap_or_default();
                apply(style, true);
            });
        }
        {
            let apply = apply.clone();
            let suppress_row = suppress_row.clone();
            icon_style_row.connect_selected_notify(move |row| {
                if suppress_row.get() {
                    return;
                }
                let style = crate::icon_style::IconStyle::ALL.get(row.selected() as usize).copied().unwrap_or_default();
                apply(style, true);
            });
        }
        // Seed the row from the pack in force without writing anything back.
        apply(initial_style, false);
    }
    // GTK4 only updates `default-width` when the window is resized while
    // resizable and not maximized/tiled/fullscreen (see `should_remember_size`
    // in gtkwindow.c), so instead we listen on the window's GdkSurface
    // `width`, which is updated on every surface resize in every state. The
    // surface only exists once the window is realized, so it's wired from the
    // window's `map` signal (guarded so re-maps don't stack handlers). The
    // width is read at timeout time - by then the new allocation has settled,
    // so the applied percentages are against the final window size. Debounced
    // so the stored percentages are applied once the window stops resizing.
    {
        let check_overview_fits = check_overview_fits.clone();
        let settings = settings.clone();
        let wired: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
        window.connect_map(move |window| {
            // Ahead of both guards below, and deliberately on every map: a
            // window restored maximized is *created* at its final size, so no
            // surface resize ever follows to trigger the reapply, and a window
            // that was hidden to the tray and shown again wants the same
            // treatment. Nothing else restores the panes at startup.
            schedule_apply_pane_widths();
            let Some(surface) = window.surface() else {
                return;
            };
            if wired.get() {
                return;
            }
            wired.set(true);
            let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
            // Width and height changes both funnel into the same debounced
            // save/reapply - a resize that changes both only needs to settle
            // once, and dragging just one edge still triggers this via
            // whichever of the two notifies fires.
            let on_notify = {
                let schedule_for_notify = schedule_apply_pane_widths.clone();
                let check_for_notify = check_overview_fits.clone();
                let settings_for_notify = settings.clone();
                let window_for_width = window.clone();
                let debounce = debounce.clone();
                move || {
                    if let Some(id) = debounce.take() {
                        id.remove();
                    }
                    let window_for_timeout = window_for_width.clone();
                    let schedule_for_timeout = schedule_for_notify.clone();
                    let check_for_timeout = check_for_notify.clone();
                    let settings_for_timeout = settings_for_notify.clone();
                    let debounce_for_timeout = debounce.clone();
                    debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                        debounce_for_timeout.set(None);
                        // Through the shared scheduler rather than straight to
                        // the apply, so a resize gets the same settle-and-retry
                        // treatment as every other trigger. The window width has
                        // stopped moving by now, but the paneds' own allocations
                        // trail it by several frames, and a single shot here
                        // would restore them against half-settled widths.
                        schedule_for_timeout();
                        check_for_timeout();
                        // Only the un-maximized size is worth restoring to -
                        // GTK4's own `default-width`/`default-height` follow
                        // the same rule (see comment above).
                        if !window_for_timeout.is_maximized() {
                            settings_for_timeout.set_int(crate::settings::WINDOW_WIDTH, window_for_timeout.width());
                            settings_for_timeout.set_int(crate::settings::WINDOW_HEIGHT, window_for_timeout.height());
                        }
                    })));
                }
            };
            {
                let on_notify = on_notify.clone();
                surface.connect_width_notify(move |_| on_notify());
            }
            surface.connect_height_notify(move |_| on_notify());
            // The window can also come up already too narrow (a restored
            // geometry) with no subsequent resize to trigger the check, so
            // run it once the initial allocation settles too.
            let check_initial = check_overview_fits.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), check_initial);
        });
    }
    // Maximize/unmaximize toggles immediately (not a drag stream like a
    // resize, so no debounce needed) - separate from the width/height save
    // above since GTK4 doesn't report a width/height surface-notify purely
    // from maximizing. The pane widths are reapplied here for the same reason:
    // the window's width has just changed by a lot, and the stored percentages
    // are of the window width, so they'd otherwise only be reapplied if some
    // surface notify happened to follow. Debounced along with every other
    // trigger, so a maximize that *does* also produce a resize notify still
    // only runs one apply.
    {
        let settings = settings.clone();
        let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
        window.connect_maximized_notify(move |win| {
            settings.set_bool(crate::settings::WINDOW_MAXIMIZED, win.is_maximized());
            schedule_apply_pane_widths();
        });
    }
    // A third width signal, because neither of the two above is guaranteed to
    // fire. The GdkSurface notify is wired from the window's `map`, so a
    // compositor that maps the window *already* at its final maximized size
    // produces no post-map surface resize at all; and `notify::maximized` can
    // arrive before the new allocation, which is the race the width-stability
    // check in the scheduler exists to survive. GtkWindow's own `default-width`
    // moves on an ordinary resize, so listening here as well means a width
    // change has to evade all three to go unnoticed. Everything shares the one
    // debounce, so the redundancy costs nothing.
    {
        let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
        window.connect_notify_local(Some("default-width"), move |_, _| {
            schedule_apply_pane_widths();
        });
    }
    // The calendar and people panes live in `root_stack`, so they're only
    // mapped while their tab is the visible one. The window-resize handler
    // above therefore skips them whenever the window is resized on another
    // tab, so the stored percentages are also reapplied each time the stack
    // switches to a page holding a paned split. The per-pane map handlers
    // cover this too - a page becoming visible maps its panes - but the page
    // switch is the cheaper, earlier signal, and both share the one debounce.
    {
        let root_stack = root_stack.clone();
        let schedule_apply_pane_widths = schedule_apply_pane_widths.clone();
        root_stack.connect_notify_local(Some("visible-child-name"), move |_, _| {
            schedule_apply_pane_widths();
        });
    }

    // --- Ctrl+F is a global keyboard shortcut now: see the shortcuts block
    // after the nav-rail buttons below (it dispatches through the same
    // `EventControllerKey` as every other chord). ---

    let state = state.clone();
    // The UI-state database also holds local-only tasks - best-effort, like
    // the contacts code's handle: a failed open means local tasks live in
    // memory for this session only.
    let local_tasks_db = UiStateDb::open()
        .map(|db| Rc::new(RefCell::new(db)))
        .inspect_err(|e| tracing::warn!("local tasks won't persist: {e}"))
        .ok();
    let local_tasks = local_tasks_db.as_ref().and_then(|db| db.borrow().load_local_tasks().ok()).unwrap_or_default();
    let calendar_state = Rc::new(RefCell::new(CalendarUiState {
        accounts: HashMap::new(),
        displayed_month: current_month_start(),
        checked_calendar_ids: HashSet::new(),
        calendar_colors: calendar_colors::load(),
        webcal_cmd_tx: None,
        webcal_subscriptions: Vec::new(),
        webcal_handles: HashMap::new(),
        birthdays: None,
        google_tasks: HashMap::new(),
        graph_todo: HashMap::new(),
        google_account_emails: Vec::new(),
        local_tasks,
        task_reminders_refresh: None,
        local_tasks_db,
        local_task_lists: state.borrow().app_config.borrow().local_task_lists(),
        tasks_filter: crate::tasks_view::TasksFilter::default(),
        calendar_search_active: false,
        calendar_search_query: String::new(),
        calendar_search_results: Vec::new(),
        calendar_search_previous_view: "month",
        calendar_search_generation: 0,
        calendar_search_pending: 0,
        calendar_search_checked_only: state.borrow().settings.get_string(crate::settings::CALENDAR_SEARCH_SCOPE) == "checked",
        calendar_search_backfilled: false,
        calendar_search_refresh: None,
        my_day_day: crate::calendar_tz::display_zone().today(),
        my_day_pins: Vec::new(),
        dashboard_refresh: None,
        flagged_mail: Vec::new(),
        dashboard_generation: 0,
        pending_calendar_moves: HashMap::new(),
        mail_overview_activate: None,
        mail_overview_refresh: None,
        scheduling_pending: HashMap::new(),
        scheduling_next_id: 0,
        pending_open_editor: None,
        open_event_editor: None,
        calendar_groups: state.borrow().app_config.borrow().calendar_groups.clone(),
        hidden_calendars: state.borrow().app_config.borrow().hidden_calendars.clone(),
        calendar_sidebar_action: None,
        eds_mirror: Default::default(),
    }));
    // The calendar handle `render_body` reaches for when an incoming iMIP
    // REPLY arrives (item 3.6): stored on `UiState` so the reply can be
    // applied without every render path carrying a calendar reference.
    state.borrow_mut().calendar_state = Some(calendar_state.clone());

    // EDS mirror (E2.3): the startup request and the once-a-day tick are
    // wired now; both are no-ops until E2.4 turns the mirror on.
    crate::eds_mirror::request_sync(&calendar_state, &worker, None);
    crate::eds_mirror::start_daily_sync(calendar_state.clone(), worker.clone());

    // My Day (item 4.9 / T-02): prune stale days and load today's pin set into
    // the in-memory mirror before the first Tasks repaint.
    {
        let day = calendar_state.borrow().my_day_day.format("%Y-%m-%d").to_string();
        // Copied out first: an `if let` scrutinee would keep the borrow alive
        // across the body, panicking on the `borrow_mut` below.
        let local_tasks_db = calendar_state.borrow().local_tasks_db.clone();
        if let Some(db) = local_tasks_db {
            let db = db.borrow();
            let _ = db.prune_my_day(&day);
            if let Ok(pins) = db.load_my_day(&day) {
                calendar_state.borrow_mut().my_day_pins = pins;
            }
        }
    }

    // --- Lookout dashboard refresh hook: the window registers one closure
    // that repaints the whole dashboard, and hands it to the mail sessions
    // (which refresh it on folder/message syncs) and into `calendar_state`
    // (where `refresh_tasks_view` and the calendar event loops reach it).
    // The tab-open and toolbar-Refresh handlers call `refresh_lookout_view`
    // directly instead (see below) - they represent the dashboard becoming
    // visible or an explicit user ask, so they must run immediately, not
    // wait out this hook's debounce or bail out on its visibility check.
    //
    // Debounced (500ms trailing edge, the pane-resize save's idiom at
    // `main_paned.connect_notify_local` above) and skipped entirely while
    // the dashboard isn't the visible tab - a sync burst across several
    // accounts would otherwise repaint the (per-account, whole-`messages`-
    // table) histogram/top-contacts scan once per event, on screen or not.
    let dashboard_refresh: Rc<dyn Fn()> = {
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let lookout_view = lookout_view.clone();
        let lookout_view_button = lookout_view_button.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        Rc::new(move || {
            if !lookout_view_button.is_active() {
                return;
            }
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let state = state.clone();
            let worker = worker.clone();
            let calendar_state = calendar_state.clone();
            let lookout_view = lookout_view.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                debounce_for_timeout.set(None);
                refresh_lookout_view(&state, &worker, &calendar_state, &lookout_view);
            })));
        })
    };
    calendar_state.borrow_mut().dashboard_refresh = Some(dashboard_refresh.clone());

    // --- Profile card context (Phase 4.2 / P-10, UX-67). Built once the
    // calendar state, the reading stack and the window all exist; the
    // message-list rows read it lazily through `profile_card_provider`.
    {
        let window_for_card = window.clone();
        let state_for_card = state.clone();
        let worker_for_card = worker.clone();
        let reading_stack_for_card = reading_stack.clone();
        let calendar_state_for_card = calendar_state.clone();
        let toast_overlay_for_card = toast_overlay.clone();

        let send_email: crate::profile_card::SendEmailFn = {
            let state = state_for_card.clone();
            let worker = worker_for_card.clone();
            let reading_stack = reading_stack_for_card.clone();
            let toast_overlay = toast_overlay_for_card.clone();
            Rc::new(move |address, preferred| {
                let chosen = {
                    let st = state.borrow();
                    preferred.filter(|id| st.accounts.contains_key(id)).or_else(|| st.accounts.keys().next().cloned())
                };
                let Some(account_id) = chosen else {
                    toast_overlay.add_toast(adw::Toast::new("Connect a mail account to send mail."));
                    return;
                };
                let Some(cmd_tx) = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone()) else {
                    return;
                };
                let prefill = crate::compose::ComposePrefill {
                    to: Some(address.address.clone()),
                    ..Default::default()
                };
                let rich_text_default = state.borrow().rich_text_default;
                show_composer_in_reading_pane(
                    &state,
                    &worker,
                    &reading_stack,
                    "New message",
                    cmd_tx,
                    prefill,
                    rich_text_default,
                    Some(account_id),
                    Some(crate::config_view::SignatureDefault::New),
                );
            })
        };

        let schedule_meeting: crate::profile_card::PersonActionFn = {
            let window = window_for_card.clone();
            let state = state_for_card.clone();
            let worker = worker_for_card.clone();
            let calendar_state = calendar_state_for_card.clone();
            let toast_overlay = toast_overlay_for_card.clone();
            let profile_card = profile_card_provider.clone();
            Rc::new(move |address| {
                let now = chrono::Local::now().naive_local();
                let rounded = now + chrono::Duration::hours(1);
                let start = rounded.with_minute(0).unwrap_or(rounded).with_second(0).unwrap_or(rounded);
                show_new_event_editor(
                    &window,
                    &state,
                    &worker,
                    &calendar_state,
                    &toast_overlay,
                    start,
                    None,
                    std::slice::from_ref(address),
                    None,
                    &profile_card,
                );
            })
        };

        let add_contact: crate::profile_card::PersonActionFn = {
            let window = window_for_card.clone();
            let state = state_for_card.clone();
            let toast_overlay = toast_overlay_for_card.clone();
            Rc::new(move |address| show_create_contact_for(&window, &state, &toast_overlay, address, None))
        };

        let edit_contact: crate::profile_card::PersonActionFn = {
            let window = window_for_card.clone();
            let state = state_for_card.clone();
            let toast_overlay = toast_overlay_for_card.clone();
            Rc::new(move |address| match find_contact_by_address(&state, &address.address, None) {
                Some((_, entry)) => show_contact_editor_for(&window, &state, &toast_overlay, &entry),
                None => show_create_contact_for(&window, &state, &toast_overlay, address, None),
            })
        };

        let open_message: crate::profile_card::OpenMessageFn = {
            let window = window_for_card.clone();
            let state = state_for_card.clone();
            let worker = worker_for_card.clone();
            let reading_stack = reading_stack_for_card.clone();
            Rc::new(move |summary| crate::message_window::open_message_window(&window, &state, &worker, &reading_stack, summary))
        };

        let open_event: crate::profile_card::OpenEventFn = {
            let window = window_for_card.clone();
            let state = state_for_card.clone();
            let worker = worker_for_card.clone();
            let calendar_state = calendar_state_for_card.clone();
            let toast_overlay = toast_overlay_for_card.clone();
            let profile_card = profile_card_provider.clone();
            Rc::new(move |occ| open_event_editor_for(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, occ))
        };

        let events_for_person: crate::profile_card::EventsForPersonFn = {
            let calendar_state = calendar_state_for_card.clone();
            Rc::new(move |address| {
                calendar_state
                    .borrow()
                    .checked_occurrences_all_months()
                    .into_iter()
                    .filter(|occ| {
                        occ.organizer.as_ref().is_some_and(|a| a.address.eq_ignore_ascii_case(address))
                            || occ.attendees.iter().any(|a| a.address.address.eq_ignore_ascii_case(address))
                    })
                    .cloned()
                    .collect()
            })
        };

        // The ribbon's Add Sender: the selected message's sender, through the
        // card's own "Add to contacts".
        {
            let add_contact = add_contact.clone();
            let message_list = message_list.clone();
            let state = state_for_card.clone();
            add_sender_button.connect_clicked(move |_| match message_list.selected_summary().and_then(|summary| summary.from.first().cloned()) {
                Some(sender) => add_contact(&sender),
                None => {
                    if let Some(overlay) = &state.borrow().toast_overlay {
                        overlay.add_toast(adw::Toast::new("Select a message first"));
                    }
                }
            });
        }
        let actions = crate::profile_card::ProfileCardActions {
            send_email,
            schedule_meeting,
            add_contact,
            edit_contact,
            open_message,
            open_event,
            events_for_person,
        };
        let context = Rc::new(crate::profile_card::ProfileCardContext {
            state: state_for_card.clone(),
            worker: worker_for_card.clone(),
            actions,
        });
        *profile_card_ctx.borrow_mut() = Some(context.clone());

        // Reading-pane header: the sender avatar and name open the card.
        let resolve: crate::profile_card::PersonResolver = {
            let message_list = message_list.clone();
            Rc::new(move || {
                message_list
                    .selected_summary()
                    .and_then(|summary| summary.from.first().map(|sender| (sender.clone(), mailbox_account_id(&summary.mailbox))))
            })
        };
        crate::profile_card::attach_person_trigger(profile_card_provider.clone(), &message_header.avatar_label, true, resolve.clone());
        crate::profile_card::attach_person_trigger(profile_card_provider.clone(), &message_header.sender_name_label, true, resolve);
    }

    // --- The More menu's "Create task from message…" (the old Follow-up
    // button's job before item 1.16 made Flag a message-level flag): opens
    // the task editor seeded from the message, defaulting into its own
    // account's task list.
    {
        let window = window.clone();
        let state_for_hook = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        state.borrow_mut().create_task_from_message = Some(Rc::new(move |summary: &EmailSummary| {
            let account_id = mailbox_account_id(&summary.mailbox);
            let account_email = account_id.as_ref().and_then(|id| state_for_hook.borrow().accounts.get(id).map(|h| h.email.clone()));
            show_create_task_for_email(&window, &state_for_hook, &calendar_state, &tasks_view, summary, account_id, account_email);
        }));
    }

    // --- The message menu's "Reply with meeting" (item 3.19 / C-20): opens a
    // new event prefilled with the conversation's participants (the user's own
    // addresses removed) and its subject, defaulting to the next whole hour.
    // Late-bound like the hook above, since the context menu is built before
    // `calendar_state` exists.
    {
        let window = window.clone();
        let state_for_hook = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        state.borrow_mut().reply_with_meeting = Some(Rc::new(move |messages: &[EmailSummary]| {
            let now = chrono::Local::now().naive_local();
            let rounded = now + chrono::Duration::hours(1);
            let start = rounded.with_minute(0).unwrap_or(rounded).with_second(0).unwrap_or(rounded);
            let self_addresses = {
                let st = state_for_hook.borrow();
                let mut addresses: Vec<String> = st.accounts.values().map(|handle| handle.email.clone()).collect();
                addresses.extend(st.app_config.borrow().identities.iter().map(|identity| identity.email.clone()));
                addresses
            };
            let participants = crate::reply_with_meeting::meeting_participants(messages, &self_addresses);
            let subject = crate::reply_with_meeting::meeting_subject(messages);
            show_new_event_editor(
                &window,
                &state_for_hook,
                &worker,
                &calendar_state,
                &toast_overlay,
                start,
                None,
                &participants,
                Some(&subject),
                &profile_card,
            );
        }));
    }

    // --- Calendar event reminders. The engine accumulates every occurrence
    // the sessions report (via `connect_calendar_account`'s ingest) and the
    // loop fires `Gio.Notification`s for due alerts, with Open / Snooze /
    // Dismiss actions; `open_event` lands on the shared event editor. The
    // persistence handle is a second best-effort open of the UI-state
    // database (the one in `UiState` is owned by the contacts code) - a
    // failed open just means fire-once state doesn't survive restarts.
    let reminders_engine = Rc::new(RefCell::new(crate::reminders::ReminderEngine::new(
        crate::ui_state_db::UiStateDb::open()
            .map(|db| Rc::new(RefCell::new(db)))
            .inspect_err(|e| tracing::warn!("reminder state won't persist: {e}"))
            .ok(),
    )));
    // Task reminders (T-05): the Tasks view's refresh funnel feeds the engine
    // once the engine exists; seed it with the current snapshot so an alert
    // due at launch fires without waiting for the first repaint.
    {
        let engine = reminders_engine.clone();
        calendar_state.borrow_mut().task_reminders_refresh = Some(Rc::new(move |tasks| engine.borrow_mut().sync_tasks(tasks)));
        reminders_engine.borrow_mut().sync_tasks(&merged_tasks_with_accounts(&calendar_state));
    }
    crate::reminders::spawn_reminder_loop(
        app,
        reminders_engine.clone(),
        settings.clone(),
        {
            let window = window.clone();
            let state = state.clone();
            let worker = worker.clone();
            let calendar_state = calendar_state.clone();
            let calendar_view_button = calendar_view_button.clone();
            let toast_overlay = toast_overlay.clone();
            let profile_card = profile_card_provider.clone();
            Rc::new(move |occ: EventOccurrence| {
                window.present();
                calendar_view_button.set_active(true);
                open_event_editor_for(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ);
            })
        },
        {
            let window = window.clone();
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let tasks_view = tasks_view.clone();
            let tasks_view_button = tasks_view_button.clone();
            Rc::new(move |task: CalendarTask| {
                window.present();
                tasks_view_button.set_active(true);
                open_task_editor_for(&window, &state, &calendar_state, &tasks_view, &task);
            })
        },
    );
    // --- Mail notification actions. `app.raise-window` and `app.open-mailbox`
    // are the click targets `mail_notifications::show_new_mail_notification`/
    // `show_send_failed_notification` set as their default action; both just
    // reuse existing navigation rather than duplicating it. Selecting the row
    // programmatically fires the same `connect_selected_item_notify` handler
    // (below) a real sidebar click does, so it gets `select_mailbox` /
    // `exit_search` / `refresh_list_header` for free.
    crate::mail_notifications::spawn_actions(
        app,
        {
            let window = window.clone();
            let folder_selection = folder_selection.clone();
            Rc::new(move |mailbox_id: MailboxId| {
                window.present();
                if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                    if let Some(index) = find_mailbox_index(&model, &mailbox_id) {
                        folder_selection.set_selected(index);
                    }
                }
            })
        },
        {
            let window = window.clone();
            Rc::new(move || window.present())
        },
    );
    // --- AI Chat link actions. `app.open-message` is the click target a
    // `lookout-action:open-message?...` link in the chat pane resolves to
    // (see the AI-Chat wiring block's `connect_decide_policy` handler
    // below). Already on the target mailbox: select the row directly,
    // since `select_mailbox` won't fire again for a same-mailbox reselect.
    // Otherwise stash the target in `pending_message_selection` (consumed
    // once `select_mailbox`'s async cache-read reply actually repaints the
    // list - see there) and switch to that mailbox exactly like the
    // `open-mailbox` closure above.
    let open_message_at: Rc<dyn Fn(MailboxId, Uid)> = {
        let window = window.clone();
        let state = state.clone();
        let folder_selection = folder_selection.clone();
        let message_list = message_list.clone();
        let mail_view_button = mail_view_button.clone();
        Rc::new(move |mailbox: MailboxId, uid: Uid| {
            window.present();
            // Switch to the Mail tab - a chat link may be clicked from the
            // Lookout tab (where the AI Chat card lives) or any other, same
            // as `calendar_view_button.set_active(true)` does for an
            // `open-event` click landing on the Calendar tab.
            mail_view_button.set_active(true);
            if state.borrow().current_mailbox.as_ref() == Some(&mailbox) {
                message_list.select_uid(&mailbox, uid);
                return;
            }
            state.borrow_mut().pending_message_selection = Some((mailbox.clone(), uid));
            if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                if let Some(index) = find_mailbox_index(&model, &mailbox) {
                    folder_selection.set_selected(index);
                } else {
                    // Not in the tree (e.g. hidden/unsubscribed) - drop the
                    // stash so it can't wrongly fire on some later,
                    // unrelated navigation.
                    state.borrow_mut().pending_message_selection = None;
                }
            }
        })
    };
    crate::chat_links::spawn_open_message_action(app, open_message_at.clone());
    // --- The Tasks view's "Flagged email" section (item 1.16): its checkbox
    // marks the flag complete, a click opens the message (the same path as a
    // chat link), and `flagged_mail_refresh` repaints it whenever any
    // account's `FlaggedMessages` lands.
    {
        let state_for_complete = state.clone();
        let message_list = message_list.clone();
        let open_message_at = open_message_at.clone();
        crate::tasks_view::set_flag_handlers(
            &tasks_view,
            Rc::new(move |summary: EmailSummary| {
                set_follow_up(&state_for_complete, &message_list, vec![summary], crate::follow_up::completed());
            }),
            Rc::new(move |summary: EmailSummary| open_message_at(summary.mailbox, summary.uid)),
        );
        let state_for_refresh = state.clone();
        let tasks_view = tasks_view.clone();
        let calendar_state = calendar_state.clone();
        state.borrow_mut().flagged_mail_refresh = Some(Rc::new(move || {
            let flagged = all_flagged_mail(&state_for_refresh);
            crate::tasks_view::set_flagged_mail(&tasks_view, &flagged);
            calendar_state.borrow_mut().flagged_mail = flagged;
            refresh_dashboard_hook(&calendar_state);
            refresh_mail_overview_hook(&calendar_state);
        }));
        // Outlook's per-message properties on Microsoft 365 accounts, re-read
        // on a timer as well as on connect and F5.
        let state = state.clone();
        glib::timeout_add_seconds_local(GRAPH_PROPERTY_RESYNC.as_secs() as u32, move || {
            let account_ids: Vec<AccountId> = state.borrow().accounts.keys().cloned().collect();
            for account_id in &account_ids {
                sync_mail_properties_from_graph(&state, account_id);
            }
            glib::ControlFlow::Continue
        });
    }
    // --- iMIP banner actions. The banner's button acts on the invitation
    // stashed in `UiState::imip` by `render_body`; the calendar half of the
    // response (saving an accepted event, removing a cancelled one) needs
    // `calendar_state`, which is why this handler is registered here rather
    // than alongside the banner widget's construction. REQUEST opens a
    // three-way response dialog, CANCEL a remove-from-calendar confirmation,
    // and REPLY is informational (the button just dismisses). Acting on the
    // banner hides it for that message, like the unsubscribe banner.
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let reading_stack_for_banner = reading_stack.clone();
        imip_banner.connect_button_clicked(move |banner| {
            let (mailbox, uid, invitation, from_email, display_name, cmd_tx) = {
                let st = state.borrow();
                let (mailbox, uid) = match &st.rendered_message {
                    Some(rendered) => rendered.clone(),
                    None => return,
                };
                let Some(invitation) = st.imip.clone() else { return };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let Some(handle) = st.accounts.get(&account_id) else { return };
                (mailbox, uid, invitation, handle.email.clone(), handle.display_name.clone(), handle.cmd_tx.clone())
            };
            let dismiss = |state: &Rc<RefCell<UiState>>, banner: &adw::Banner, mailbox: &MailboxId, uid: Uid| {
                state.borrow_mut().imip_dismissed = Some((mailbox.clone(), uid));
                banner.set_revealed(false);
            };
            match invitation.method {
                lookout_core::ImipMethod::Request => {
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Invitation: {}", invitation.summary.as_deref().unwrap_or("an event")))
                        .body(if let Some(organizer) = &invitation.organizer {
                            format!("Responding will send your answer to {}.", organizer.display_label())
                        } else {
                            "Responding will send your answer to the organizer.".to_string()
                        })
                        .default_response("accept")
                        .close_response("cancel")
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("decline", "Decline");
                    dialog.add_response("tentative", "Tentative");
                    dialog.add_response("accept", "Accept");
                    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
                    dialog.set_response_appearance("decline", adw::ResponseAppearance::Destructive);
                    let state_for_dialog = state.clone();
                    let calendar_state_for_dialog = calendar_state.clone();
                    let toast_overlay_for_dialog = toast_overlay.clone();
                    let cmd_tx_for_dialog = cmd_tx.clone();
                    let from_email_for_dialog = from_email.clone();
                    let display_name_for_dialog = display_name.clone();
                    let invitation_for_dialog = invitation.clone();
                    let mailbox_for_dialog = mailbox.clone();
                    let banner_for_dialog = banner.clone();
                    dialog.connect_response(None, move |_dialog, response| {
                        let status = match response {
                            "accept" => Some(lookout_core::AttendeeStatus::Accepted),
                            "tentative" => Some(lookout_core::AttendeeStatus::Tentative),
                            "decline" => Some(lookout_core::AttendeeStatus::Declined),
                            _ => None,
                        };
                        let Some(status) = status else { return };
                        respond_to_imip_invitation(
                            &calendar_state_for_dialog,
                            &toast_overlay_for_dialog,
                            &invitation_for_dialog,
                            &from_email_for_dialog,
                            Some(display_name_for_dialog.as_str()),
                            &cmd_tx_for_dialog,
                            status,
                            true,
                            None,
                        );
                        state_for_dialog.borrow_mut().imip_dismissed = Some((mailbox_for_dialog.clone(), uid));
                        banner_for_dialog.set_revealed(false);
                    });
                    dialog.present(Some(banner));
                }
                lookout_core::ImipMethod::Cancel => {
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Cancelled: {}", invitation.summary.as_deref().unwrap_or("an event")))
                        .body("The organizer cancelled this event. Remove it from your calendar?")
                        .default_response("keep")
                        .close_response("keep")
                        .build();
                    dialog.add_response("keep", "Keep");
                    dialog.add_response("remove", "Remove from calendar");
                    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                    let state_for_dialog = state.clone();
                    let calendar_state_for_dialog = calendar_state.clone();
                    let toast_overlay_for_dialog = toast_overlay.clone();
                    let invitation_for_dialog = invitation.clone();
                    let mailbox_for_dialog = mailbox.clone();
                    let banner_for_dialog = banner.clone();
                    dialog.connect_response(None, move |_dialog, response| {
                        dismiss(&state_for_dialog, &banner_for_dialog, &mailbox_for_dialog, uid);
                        if response == "remove" {
                            remove_cancelled_imip_event(&calendar_state_for_dialog, &toast_overlay_for_dialog, &invitation_for_dialog);
                        }
                    });
                    dialog.present(Some(banner));
                }
                lookout_core::ImipMethod::Reply => {
                    dismiss(&state, banner, &mailbox, uid);
                }
                lookout_core::ImipMethod::Counter => {
                    // CAL-17: an attendee proposed a new time. Accepting
                    // updates the meeting and mails everyone the update;
                    // declining answers with a DECLINECOUNTER - the same two
                    // decisions as the invite card's buttons.
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("New time proposed: {}", invitation.summary.as_deref().unwrap_or("an event")))
                        .body("Accepting updates the meeting to the proposed time and sends it to everyone.")
                        .default_response("accept")
                        .close_response("cancel")
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("decline", "Decline proposal");
                    dialog.add_response("accept", "Accept proposal");
                    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
                    dialog.set_response_appearance("decline", adw::ResponseAppearance::Destructive);
                    let state_for_dialog = state.clone();
                    let calendar_state_for_dialog = calendar_state.clone();
                    let toast_overlay_for_dialog = toast_overlay.clone();
                    let invitation_for_dialog = invitation.clone();
                    let mailbox_for_dialog = mailbox.clone();
                    let banner_for_dialog = banner.clone();
                    let reading_stack_for_dialog = reading_stack_for_banner.clone();
                    dialog.connect_response(None, move |_dialog, response| {
                        let accepted = match response {
                            "accept" => true,
                            "decline" => false,
                            _ => return,
                        };
                        let acted = if accepted {
                            accept_counter_proposal(&state_for_dialog, &calendar_state_for_dialog, &toast_overlay_for_dialog, &invitation_for_dialog)
                        } else {
                            decline_counter_proposal(&state_for_dialog, &calendar_state_for_dialog, &toast_overlay_for_dialog, &invitation_for_dialog)
                        };
                        if acted {
                            dismiss_imip(&state_for_dialog, &reading_stack_for_dialog, &mailbox_for_dialog, uid);
                            banner_for_dialog.set_revealed(false);
                        }
                    });
                    dialog.present(Some(banner));
                }
                lookout_core::ImipMethod::DeclineCounter => {
                    // Informational: the organizer turned down our proposal.
                    dismiss_imip(&state, &reading_stack_for_banner, &mailbox, uid);
                }
            }
        });
    }
    // The invite card's own response buttons (CAL-15 / UX-60): wired here,
    // where the calendar state exists, with the same per-message lookups the
    // banner dialog above uses. `render_invite_card` shows the row for an
    // un-dismissed REQUEST only.
    build_imip_response_actions(&state, &calendar_state, &toast_overlay, &worker, &reading_stack);
    // Which single day the Mail-screen overview pane's day list is
    // currently showing - separate from `calendar_state.displayed_month`
    // (that's the main Calendar view's own concern).
    let mail_overview_day: Rc<Cell<chrono::NaiveDate>> = Rc::new(Cell::new(crate::calendar_tz::display_zone().today()));
    refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);

    // The overview panel's "Upcoming events" section needs the 90-day
    // window fetched; switching to the Mail screen widens it, the same way
    // the Lookout tab open widens it for its own upcoming card.
    {
        let calendar_state = calendar_state.clone();
        mail_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                widen_calendar_sync_horizon(&calendar_state);
            }
        });
    }

    // --- Mail-screen overview pane's task rows + repaint hook, registered
    // here (same pattern as `dashboard_refresh`/`task_button_refresh` just
    // above) so `refresh_mail_overview_day_list` can build task rows - a row
    // click opens the shared task editor, reusing the Tasks view's session
    // paths (the overview's rows carry no completion checkbox). The pane's
    // repaint hook fires from `refresh_tasks_view`, so every task change
    // (synced, saved, toggled, deleted) updates the pane.
    {
        let calendar_state_for_activate = calendar_state.clone();
        let tasks_view_for_activate = tasks_view.clone();
        let mail_overview_refresh: Rc<dyn Fn()> = {
            let calendar_state = calendar_state.clone();
            let mail_overview_day = mail_overview_day.clone();
            let mail_overview_day_list = mail_overview_day_list.clone();
            Rc::new(move || refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list))
        };
        let mut st = calendar_state.borrow_mut();
        st.mail_overview_refresh = Some(mail_overview_refresh);
        let window_for_activate = window.clone();
        let state_for_activate = state.clone();
        st.mail_overview_activate = Some(Rc::new(move |task| {
            open_task_editor_for(&window_for_activate, &state_for_activate, &calendar_state_for_activate, &tasks_view_for_activate, &task)
        }));
    }

    let contacts_categories: Rc<RefCell<Vec<ContactsCategoryChoice>>> = Rc::new(RefCell::new(Vec::new()));
    let contacts_entries: Rc<RefCell<Vec<ContactsListEntry>>> = Rc::new(RefCell::new(Vec::new()));
    // Which contacts are checked in the right-hand list, keyed the same way
    // as `starred_contacts` (`(account_id, identity)`), so a selection
    // survives the list's full rebuilds. Pruned to the visible entries inside
    // `rebuild_contacts_list_ui`; drives the ribbon's Edit/Delete via
    // `refresh_contacts_actions`.
    let contacts_selected: Rc<RefCell<HashSet<(AccountId, String)>>> = Rc::new(RefCell::new(HashSet::new()));
    let refresh_contacts_actions: Rc<dyn Fn()> = Rc::new({
        let contacts_selected = contacts_selected.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_edit_button = contacts_edit_button.clone();
        let contacts_delete_button = contacts_delete_button.clone();
        let contacts_email_button = contacts_email_button.clone();
        move || {
            let entries = contacts_entries.borrow();
            let selected_entries: Vec<ContactsListEntry> = entries
                .iter()
                .filter(|entry| contacts_selected.borrow().contains(&(entry.account_id.clone(), contact_identity(entry.card()))))
                .cloned()
                .collect();
            // Edit needs exactly one selected contact; Delete any number.
            // Deleted-bucket cards (`href` empty) are display-only and can be
            // neither edited nor deleted, so they don't arm either button.
            let single = match selected_entries.len() {
                1 => selected_entries.first(),
                _ => None,
            };
            contacts_edit_button.set_sensitive(single.is_some_and(|entry| !entry.href().is_empty()));
            contacts_email_button.set_sensitive(single.is_some_and(|entry| !entry.card().emails.is_empty()));
            contacts_delete_button.set_sensitive(!selected_entries.is_empty() && selected_entries.iter().any(|entry| !entry.href().is_empty()));
        }
    });

    // The details pane's actions (UX-65): the composer / event / message
    // wiring is reused from the profile card's context (resolved lazily, so a
    // trigger installed before the context exists still works), plus the
    // pane's own inline edit, delete and URI opening.
    let contacts_details_actions: crate::contacts_details::ContactsDetailsActions = {
        let send_email: crate::profile_card::SendEmailFn = {
            let provider = profile_card_provider.clone();
            Rc::new(move |address, account| {
                if let Some(ctx) = provider() {
                    (ctx.actions.send_email)(address, account);
                }
            })
        };
        let schedule_meeting: crate::profile_card::PersonActionFn = {
            let provider = profile_card_provider.clone();
            Rc::new(move |address| {
                if let Some(ctx) = provider() {
                    (ctx.actions.schedule_meeting)(address);
                }
            })
        };
        let open_message: crate::profile_card::OpenMessageFn = {
            let provider = profile_card_provider.clone();
            Rc::new(move |summary| {
                if let Some(ctx) = provider() {
                    (ctx.actions.open_message)(summary);
                }
            })
        };
        let events_for_person: crate::profile_card::EventsForPersonFn = {
            let provider = profile_card_provider.clone();
            Rc::new(move |address| provider().map(|ctx| (ctx.actions.events_for_person)(address)).unwrap_or_default())
        };
        let edit: Rc<dyn Fn(&ContactsListEntry)> = {
            let contacts_details = contacts_details.clone();
            let state = state.clone();
            let toast_overlay = toast_overlay.clone();
            Rc::new(move |entry: &ContactsListEntry| {
                // Desktop contacts (E1.5) are read-only here; GNOME Contacts edits them.
                if crate::desktop_contacts::is_desktop_entry(entry) {
                    crate::desktop_contacts::open_in_gnome_contacts(entry, &toast_overlay);
                    return;
                }
                let details = contacts_details.clone();
                let entry_owned = entry.clone();
                let on_save = {
                    let details = details.clone();
                    let state = state.clone();
                    let toast = toast_overlay.clone();
                    let entry = entry_owned.clone();
                    move |card: lookout_core::VCard| {
                        crate::contacts_view::save_contact_entry(&state, &toast, &entry, card);
                        details.show_details();
                        if let Some(refresh) = state.borrow().contacts_details_refresh.clone() {
                            refresh();
                        }
                    }
                };
                let on_delete = {
                    let details = details.clone();
                    let state = state.clone();
                    let toast = toast_overlay.clone();
                    let entry = entry_owned.clone();
                    move || {
                        delete_contact_entries(&state, &toast, std::slice::from_ref(&entry));
                        details.show_details();
                        if let Some(refresh) = state.borrow().contacts_details_refresh.clone() {
                            refresh();
                        }
                    }
                };
                let on_cancel = {
                    let details = details.clone();
                    move || details.show_details()
                };
                details.begin_edit(entry, on_save, on_delete, on_cancel);
            })
        };
        let delete: Rc<dyn Fn(&ContactsListEntry)> = {
            let state = state.clone();
            let toast_overlay = toast_overlay.clone();
            Rc::new(move |entry: &ContactsListEntry| {
                delete_contact_entries(&state, &toast_overlay, std::slice::from_ref(entry));
                if let Some(refresh) = state.borrow().contacts_details_refresh.clone() {
                    refresh();
                }
            })
        };
        let open_uri: Rc<dyn Fn(&str)> = Rc::new(|uri: &str| crate::window::open_uri_in_default_browser(uri));
        crate::contacts_details::ContactsDetailsActions {
            send_email,
            schedule_meeting,
            open_message,
            events_for_person,
            edit,
            delete,
            open_uri,
        }
    };

    // Repaints the details pane for whichever contact is selected in the
    // middle list (UX-65). Reads the row's stashed index (letter headers mean
    // the widget position no longer matches), and is skipped while the inline
    // editor is open so a background sync can't discard the form.
    let contacts_details_refresh: Rc<dyn Fn()> = Rc::new({
        let contacts_details = contacts_details.clone();
        let contacts_list = contacts_list.clone();
        let contacts_entries = contacts_entries.clone();
        let state = state.clone();
        let worker = worker.clone();
        let actions = contacts_details_actions.clone();
        move || {
            if contacts_details.is_editing() {
                return;
            }
            let entry = contacts_list
                .selected_row()
                .and_then(|row| unsafe { row.data::<usize>("contacts-entry-index") })
                .map(|idx| unsafe { *idx.as_ref() })
                .and_then(|idx| contacts_entries.borrow().get(idx).cloned());
            contacts_details.set_contact(entry.as_ref(), &state, &worker, &actions);
        }
    });
    state.borrow_mut().contacts_details_refresh = Some(contacts_details_refresh.clone());
    {
        let contacts_details_refresh = contacts_details_refresh.clone();
        contacts_list.connect_row_selected(move |_list, _row| contacts_details_refresh());
    }

    {
        let contacts_categories = contacts_categories.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_list = contacts_list.clone();
        let state = state.clone();
        let contacts_selected = contacts_selected.clone();
        let refresh_contacts_actions = refresh_contacts_actions.clone();
        let contacts_details_refresh = contacts_details_refresh.clone();
        let contacts_heading_label = contacts_heading_label.clone();
        contacts_category_list.connect_row_selected(move |_list, row| {
            let Some(row) = row else { return };
            // Header rows ("gavindi@outlook.com", "Categories") sit in the
            // list alongside selectable rows, so a row's position in the
            // widget no longer matches its index in `contacts_categories` -
            // that index is stashed on the row itself in
            // `refresh_contacts_category_ui`.
            let Some(idx) = (unsafe { row.data::<usize>("contacts-choice-index") }) else { return };
            let idx = unsafe { *idx.as_ref() };
            contacts_heading_label.set_label(&contacts_categories.borrow()[idx].label());
            rebuild_contacts_list_ui(
                &contacts_list,
                &contacts_categories,
                &contacts_entries,
                idx as i32,
                &state,
                &contacts_selected,
                &refresh_contacts_actions,
                &contacts_details_refresh,
            );
        });
    }
    let refresh_contacts_ui: Rc<dyn Fn(Option<i32>)> = Rc::new({
        let state = state.clone();
        let contacts_category_list = contacts_category_list.clone();
        let contacts_list = contacts_list.clone();
        let contacts_categories = contacts_categories.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_selected = contacts_selected.clone();
        let refresh_contacts_actions = refresh_contacts_actions.clone();
        let contacts_details_refresh = contacts_details_refresh.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_sidebar.calendar_list_box.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let reminders_engine = reminders_engine.clone();
        move |selected_index: Option<i32>| {
            refresh_contacts_category_ui(
                &state,
                &contacts_category_list,
                &contacts_list,
                &contacts_categories,
                &contacts_entries,
                selected_index,
                &contacts_selected,
                &refresh_contacts_actions,
                &contacts_details_refresh,
            );
            // Contacts are the birthdays calendar's only data source, so
            // every snapshot (startup cache paint, poll tick, post-write
            // resync) refreshes the synthesized calendar and everything that
            // renders it - the same funnel the webcal session uses.
            if refresh_birthdays_from_contacts(&state, &calendar_state, &reminders_engine) {
                refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                refresh_dashboard_hook(&calendar_state);
                let displayed = calendar_state.borrow().displayed_month;
                if calendar_view::mini_month(&mini_calendar) == displayed {
                    let event_days = calendar_event_days(&calendar_state, displayed);
                    calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                }
            }
        }
    });
    // The contacts repaint hook (P-06): reads the current bucket selection so
    // a filter change repaints the list in place instead of jumping back to
    // the first bucket. Stored on the shared state so the module-aware
    // title-bar search can drive it too.
    {
        let contacts_category_list = contacts_category_list.clone();
        let refresh_contacts_ui = refresh_contacts_ui.clone();
        state.borrow_mut().contacts_refresh = Some(Rc::new(move || {
            let idx = contacts_category_list
                .selected_row()
                .and_then(|row| unsafe { row.data::<usize>("contacts-choice-index") })
                .map(|idx| unsafe { *idx.as_ref() } as i32);
            refresh_contacts_ui(idx);
        }));
    }
    // The list-only repaint: the search box, the Filter/Sort menu and "File
    // contacts as" change only what the middle list shows, so they rebuild
    // just that list for the selected bucket and leave the bucket rail (and
    // the birthdays calendar) alone. With no bucket selected (nothing synced
    // yet) it falls back to the full repaint.
    {
        let state_for_hook = state.clone();
        let state = state.clone();
        let contacts_category_list = contacts_category_list.clone();
        let contacts_list = contacts_list.clone();
        let contacts_categories = contacts_categories.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_selected = contacts_selected.clone();
        let refresh_contacts_actions = refresh_contacts_actions.clone();
        let contacts_details_refresh = contacts_details_refresh.clone();
        let refresh_contacts_ui = refresh_contacts_ui.clone();
        state_for_hook.borrow_mut().contacts_list_refresh = Some(Rc::new(move || {
            let idx = contacts_category_list
                .selected_row()
                .and_then(|row| unsafe { row.data::<usize>("contacts-choice-index") })
                .map(|idx| unsafe { *idx.as_ref() } as i32);
            match idx {
                Some(idx) => rebuild_contacts_list_ui(
                    &contacts_list,
                    &contacts_categories,
                    &contacts_entries,
                    idx,
                    &state,
                    &contacts_selected,
                    &refresh_contacts_actions,
                    &contacts_details_refresh,
                ),
                None => refresh_contacts_ui(None),
            }
        }));
    }
    {
        let state = state.clone();
        contacts_search_entry.connect_search_changed(move |entry| {
            state.borrow_mut().contacts_filter = entry.text().to_string();
            let refresh = state.borrow().contacts_list_refresh.clone();
            if let Some(refresh) = refresh {
                refresh();
            }
        });
    }
    refresh_contacts_ui(None);
    // The Filter/Sort ▾ menu (UX-65), rebuilt on each open so the category
    // list is always current. Sort writes the shared "File contacts as"
    // preference; the filters live on `contacts_filter_options` and repaint
    // the list in place via the `contacts_list_refresh` hook.
    {
        let state = state.clone();
        let button = contacts_filter_button.clone();
        let popover = gtk::Popover::builder().autohide(true).has_arrow(true).build();
        popover.set_parent(&button);
        button.connect_clicked(move |_| {
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(10)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(10)
                .margin_end(10)
                .width_request(240)
                .build();

            let sort_heading = gtk::Label::builder().label("Sort by").xalign(0.0).css_classes(["heading"]).build();
            crate::privacy::mark_ui(&sort_heading);
            content.append(&sort_heading);
            let sort_dropdown = gtk::DropDown::from_strings(&crate::settings::CONTACT_FILE_AS_LABELS);
            sort_dropdown.set_selected(crate::settings::contact_file_as_index(
                &state.borrow().settings.get_string(crate::settings::CONTACTS_FILE_AS),
            ));
            {
                let state = state.clone();
                sort_dropdown.connect_selected_notify(move |dropdown| {
                    let value = crate::settings::contact_file_as_at(dropdown.selected());
                    state.borrow().settings.set_string(crate::settings::CONTACTS_FILE_AS, value);
                    let refresh = state.borrow().contacts_list_refresh.clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                });
            }
            content.append(&sort_dropdown);
            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

            let filter_heading = gtk::Label::builder().label("Filter").xalign(0.0).css_classes(["heading"]).build();
            crate::privacy::mark_ui(&filter_heading);
            content.append(&filter_heading);
            let mut categories: Vec<String> = state
                .borrow()
                .contacts_by_account
                .values()
                .flat_map(|account| account.contacts.iter())
                .flat_map(|record| record.card.categories.iter().cloned())
                .collect();
            categories.sort_by_key(|name| name.to_lowercase());
            categories.dedup();
            let mut labels = vec!["All categories".to_string()];
            labels.extend(categories.iter().cloned());
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let category_dropdown = gtk::DropDown::from_strings(&label_refs);
            // Category names are content: blurred by privacy mode.
            crate::privacy::mark_content(&category_dropdown);
            let selected_category = state.borrow().contacts_filter_options.category.clone();
            let selected_index = selected_category
                .as_ref()
                .and_then(|current| categories.iter().position(|name| name.eq_ignore_ascii_case(current)))
                .map(|position| position as u32 + 1)
                .unwrap_or(0);
            category_dropdown.set_selected(selected_index);
            {
                let state = state.clone();
                let categories = categories.clone();
                category_dropdown.connect_selected_notify(move |dropdown| {
                    let selected = dropdown.selected();
                    let value = if selected == 0 { None } else { categories.get(selected as usize - 1).cloned() };
                    state.borrow_mut().contacts_filter_options.category = value;
                    let refresh = state.borrow().contacts_list_refresh.clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                });
            }
            content.append(&category_dropdown);

            let has_email = gtk::Switch::builder()
                .valign(gtk::Align::Center)
                .active(state.borrow().contacts_filter_options.has_email)
                .build();
            {
                let state = state.clone();
                has_email.connect_active_notify(move |switch| {
                    state.borrow_mut().contacts_filter_options.has_email = switch.is_active();
                    let refresh = state.borrow().contacts_list_refresh.clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                });
            }
            content.append(&contacts_filter_switch_row("Has email", &has_email));

            let has_phone = gtk::Switch::builder()
                .valign(gtk::Align::Center)
                .active(state.borrow().contacts_filter_options.has_phone)
                .build();
            {
                let state = state.clone();
                has_phone.connect_active_notify(move |switch| {
                    state.borrow_mut().contacts_filter_options.has_phone = switch.is_active();
                    let refresh = state.borrow().contacts_list_refresh.clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                });
            }
            content.append(&contacts_filter_switch_row("Has phone", &has_phone));

            let clear = gtk::Button::with_label("Clear filters");
            {
                let state = state.clone();
                let popover = popover.clone();
                clear.connect_clicked(move |_| {
                    state.borrow_mut().contacts_filter_options = crate::contacts_view::ContactsFilterOptions::default();
                    let refresh = state.borrow().contacts_list_refresh.clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                    popover.popdown();
                });
            }
            content.append(&clear);

            popover.set_child(Some(&content));
            popover.popup();
        });
    }
    {
        let send_email = contacts_details_actions.send_email.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_selected = contacts_selected.clone();
        contacts_email_button.connect_clicked(move |_| {
            // Enabled only for exactly one selected contact with an address.
            let address = contacts_entries
                .borrow()
                .iter()
                .find(|entry| contacts_selected.borrow().contains(&(entry.account_id.clone(), contact_identity(entry.card()))))
                .and_then(|entry| entry.card().emails.first().map(|email| email.address.clone()));
            if let Some(address) = address {
                send_email(&lookout_core::EmailAddress { name: None, address }, None);
            }
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_selected = contacts_selected.clone();
        contacts_edit_button.connect_clicked(move |_| {
            // Enabled only when exactly one contact is checked (see
            // `refresh_contacts_actions`), so the first selected entry is the
            // one to edit.
            let Some(entry) = contacts_entries
                .borrow()
                .iter()
                .find(|entry| contacts_selected.borrow().contains(&(entry.account_id.clone(), contact_identity(entry.card()))))
                .cloned()
            else {
                return;
            };
            if entry.href().is_empty() {
                show_contact_details_dialog(&window, &entry);
            } else {
                show_contact_editor_for(&window, &state, &toast_overlay, &entry);
            }
        });
    }
    {
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_selected = contacts_selected.clone();
        contacts_delete_button.connect_clicked(move |_| {
            let entries: Vec<ContactsListEntry> = contacts_entries
                .borrow()
                .iter()
                .filter(|entry| contacts_selected.borrow().contains(&(entry.account_id.clone(), contact_identity(entry.card()))))
                .cloned()
                .collect();
            delete_contact_entries(&state, &toast_overlay, &entries);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let contacts_entries = contacts_entries.clone();
        contacts_list.connect_row_activated(move |_list, row| {
            let Some(entry) = (unsafe { row.data::<usize>("contacts-entry-index") })
                .map(|idx| unsafe { *idx.as_ref() })
                .and_then(|idx| contacts_entries.borrow().get(idx).cloned())
            else {
                return;
            };
            if crate::desktop_contacts::is_desktop_entry(&entry) {
                // A desktop contact's "open for editing" is GNOME Contacts (E1.5).
                crate::desktop_contacts::open_in_gnome_contacts(&entry, &toast_overlay);
            } else if entry.href().is_empty() {
                // Deleted-bucket cards are display-only (their server
                // metadata is gone with the card) - keep the read-only
                // details dialog for them.
                show_contact_details_dialog(&window, &entry);
            } else {
                show_contact_editor_for(&window, &state, &toast_overlay, &entry);
            }
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        new_contact_button.connect_clicked(move |_| {
            show_new_contact_editor(&window, &state, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        manage_groups_button.connect_clicked(move |_| {
            show_manage_groups_dialog(&window, &state, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        import_contacts_button.connect_clicked(move |_| {
            show_contacts_import_dialog(&window, &state, &toast_overlay);
        });
    }
    {
        // The Export menu's two actions. They live in a group inserted on
        // the menu button so the popover items resolve `contacts-export.*`
        // without touching the window's action namespace.
        let actions = gio::SimpleActionGroup::new();
        let export_vcard = gio::SimpleAction::new("vcard", None);
        {
            let window = window.clone();
            let contacts_entries = contacts_entries.clone();
            let toast_overlay = toast_overlay.clone();
            export_vcard.connect_activate(move |_, _| {
                export_current_contacts(&window, ContactFileFormat::VCard, &contacts_entries, &toast_overlay);
            });
        }
        let export_csv = gio::SimpleAction::new("csv", None);
        {
            let window = window.clone();
            let contacts_entries = contacts_entries.clone();
            let toast_overlay = toast_overlay.clone();
            export_csv.connect_activate(move |_, _| {
                export_current_contacts(&window, ContactFileFormat::OutlookCsv, &contacts_entries, &toast_overlay);
            });
        }
        actions.add_action(&export_vcard);
        actions.add_action(&export_csv);
        export_contacts_button.insert_action_group("contacts-export", Some(&actions));
    }

    // --- Config view: opened from the header bar's Settings button as a
    // modal dialog (see the handler near the end of this function), a
    // read-only overview of the connected Mail/Calendar accounts (endpoints
    // included, so it shows how each account is configured) plus the Phase 5
    // placeholder sections, and an "Add account" entry that opens GOA
    // settings - same invocation as the empty-state page's button. The
    // account groups are repopulated by `refresh_config` on every open and
    // again whenever either discovery lands (`spawn_*_discovery` below).
    // `config_view` itself is built earlier, alongside `contacts_paned` -
    // see the comment there.

    // Config → Appearance → "Animate transitions": flips the reading pane's
    // crossfade on/off live. Session-only state until Phase 5's GSettings
    // lands; off sets the transition type to `None`, which also makes
    // `render_body` skip its fade-specific dance (see below).
    {
        let reading_stack = reading_stack.clone();
        let state = state.clone();
        config_view.animations_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::ANIMATE_TRANSITIONS, row.is_active());
            let transition = if row.is_active() {
                gtk::StackTransitionType::Crossfade
            } else {
                gtk::StackTransitionType::None
            };
            reading_stack.set_transition_type(transition);
        });
    }

    // Config → Appearance → "Theme" / "Custom accent color": live re-theme.
    // Every change writes through to GSettings and re-applies the theme stack
    // (base palette + selected theme + accent), so the effect is immediate.
    // The accent picker only feeds the stack while its switch is on; the
    // switch's own handler re-applies with the system accent when turned off.
    {
        let theme_manager = theme_manager.clone();
        let state = state.clone();
        config_view.theme_row.connect_selected_notify(move |row| {
            let theme_id = crate::theme::theme_at(row.selected());
            state.borrow().settings.set_string(crate::theme::THEME_ID_KEY, theme_id);
            let accent = state.borrow().settings.get_string(crate::theme::ACCENT_COLOR_KEY);
            theme_manager.apply(theme_id, Some(&accent));
        });
    }

    // View ▸ Layout ▸ Density and Config → Appearance → "Density" (UX-83)
    // share this one apply path: it swaps the `spacing-*` CSS class on the
    // folder and message list panes, so every row in both reflows live (see
    // the `.spacing-*` rules in `data/styles/density.css`), and writes the choice
    // through to GSettings like the other Config rows so it survives
    // restarts. The stateful `win.density` action and the ComboRow are each
    // written behind a suppression guard, so neither can drive the other in a
    // loop - the same pattern as the reading-pane control above.
    {
        let density_action = density_action.clone();
        let density_button = density_button.clone();
        let spacing_row = config_view.spacing_row.clone();
        let folder_card = folder_card.clone().upcast::<gtk::Widget>();
        let message_list_view = message_list_view.clone().upcast::<gtk::Widget>();
        let panes = vec![folder_card, message_list_view];
        let state = state.clone();
        let suppress_row: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let apply: ApplyDensity = {
            let density_action = density_action.clone();
            let density_button = density_button.clone();
            let spacing_row = spacing_row.clone();
            let panes = panes.clone();
            let state = state.clone();
            let suppress_row = suppress_row.clone();
            Rc::new(move |choice: &str, persist: bool| {
                // Normalise first: an unknown value means Medium everywhere,
                // as it does for the CSS class and the ComboRow index.
                let value = crate::settings::spacing_at(crate::settings::spacing_index(choice));
                density_action.set_state(&value.to_variant());
                density_button.set_tooltip_text(Some(&format!("Density: {}", crate::settings::spacing_label(value))));
                let store = state.borrow().settings.clone();
                if persist {
                    store.set_string(crate::settings::LAYOUT_SPACING, value);
                }
                for pane in &panes {
                    for class in ["spacing-tight", "spacing-medium", "spacing-loose"] {
                        pane.remove_css_class(class);
                    }
                    pane.add_css_class(crate::settings::spacing_class(value));
                }
                suppress_row.set(true);
                spacing_row.set_selected(crate::settings::spacing_index(value));
                suppress_row.set(false);
            })
        };
        {
            let apply = apply.clone();
            density_action.connect_activate(move |_action, parameter| {
                let Some(choice) = parameter.and_then(|p| p.str()) else { return };
                apply(choice, true);
            });
        }
        {
            let apply = apply.clone();
            let suppress_row = suppress_row.clone();
            config_view.spacing_row.connect_selected_notify(move |row| {
                if suppress_row.get() {
                    return;
                }
                apply(crate::settings::spacing_at(row.selected()), true);
            });
        }
        // Seed both controls from the stored value without writing anything
        // back through.
        let initial = state.borrow().settings.get_string(crate::settings::LAYOUT_SPACING);
        apply(&initial, false);
    }
    // Config → Appearance → "Message preview": how many body-preview lines
    // each message-list row shows. Rewrites the live rows in place through the
    // row registry, so the list reflows as the choice changes.
    {
        let state = state.clone();
        let row_display_registry = row_display_registry.clone();
        let row_display = row_display.clone();
        config_view.preview_lines_row.connect_selected_notify(move |row| {
            let lines = row.selected() as i32;
            state.borrow().settings.set_int(crate::settings::LAYOUT_PREVIEW_LINES, lines);
            let display = RowDisplay {
                preview_lines: lines,
                ..row_display.get()
            };
            row_display.set(display);
            update_row_display(&row_display_registry, display);
        });
    }
    // Config → Appearance → "Show sender images": toggles the message-list
    // avatars (and the reading-pane header's, on the next message).
    {
        let state = state.clone();
        let row_display_registry = row_display_registry.clone();
        let row_display = row_display.clone();
        config_view.show_sender_images_row.connect_active_notify(move |row| {
            let show_sender_images = row.is_active();
            state.borrow().settings.set_bool(crate::settings::MAIL_SHOW_SENDER_IMAGES, show_sender_images);
            let display = RowDisplay {
                show_sender_images,
                ..row_display.get()
            };
            row_display.set(display);
            update_row_display(&row_display_registry, display);
        });
    }
    {
        let theme_manager = theme_manager.clone();
        let state = state.clone();
        let accent_color_row = config_view.accent_color_row.clone();
        let accent_color_button = config_view.accent_color_button.clone();
        config_view.accent_switch_row.connect_active_notify(move |row| {
            accent_color_row.set_sensitive(row.is_active());
            let stored = if row.is_active() {
                crate::theme::rgba_to_stored(&accent_color_button.rgba())
            } else {
                String::new()
            };
            state.borrow().settings.set_string(crate::theme::ACCENT_COLOR_KEY, &stored);
            let theme_id = state.borrow().settings.get_string(crate::theme::THEME_ID_KEY);
            theme_manager.apply(&theme_id, Some(&stored));
        });
    }
    {
        let theme_manager = theme_manager.clone();
        let state = state.clone();
        let accent_switch_row = config_view.accent_switch_row.clone();
        config_view.accent_color_button.connect_rgba_notify(move |button| {
            if !accent_switch_row.is_active() {
                return;
            }
            let stored = crate::theme::rgba_to_stored(&button.rgba());
            state.borrow().settings.set_string(crate::theme::ACCENT_COLOR_KEY, &stored);
            let theme_id = state.borrow().settings.get_string(crate::theme::THEME_ID_KEY);
            theme_manager.apply(&theme_id, Some(&stored));
        });
    }

    // Config → Appearance → "Dark message theme": the default state for
    // *newly-opened* messages only - the reading pane's per-message
    // "Switch message theme" toggle still overrides the current message
    // either way, so the change applies on the next navigation rather than
    // re-rendering (and possibly yanking) the message on screen.
    {
        let state = state.clone();
        config_view.message_theme_dark_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_MESSAGE_THEME_DARK, row.is_active());
        });
    }

    // Config → Mail → "Load images from the web": flips the WebView's
    // remote-image veto live, then re-renders whatever's on the reading pane
    // so the change applies to the open message, not just the next selection.
    // Skipped while a composer is up - `render_body` would route the pane
    // back to the message page and yank the user out of their draft.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        let message_list = message_list.clone();
        config_view.remote_images_row.connect_active_notify(move |row| {
            state.borrow_mut().load_remote_images = row.is_active();
            state.borrow().settings.set_bool(crate::settings::MAIL_LOAD_REMOTE_IMAGES, row.is_active());
            rerender_current_message(&state, &reading_stack, &message_header, &message_list);
        });
    }

    // Config → Mail → "Internal domains" (item 6.2 / S-06): split the
    // comma/whitespace-separated entry into the `mail-internal-domains`
    // string list. The reading pane's external-sender tag reads it on the
    // next render (it isn't re-rendered per keystroke).
    {
        let state = state.clone();
        config_view.internal_domains_entry.connect_changed(move |entry| {
            let domains = crate::settings::parse_domain_list(&entry.text());
            state.borrow().settings.set_strv(crate::settings::MAIL_INTERNAL_DOMAINS, domains);
        });
    }

    // Config → Mail → "Report phishing address" (item 6.7 / S-09): the
    // address Report phishing forwards to. Empty hides the action.
    {
        let state = state.clone();
        config_view.report_phishing_entry.connect_changed(move |entry| {
            state.borrow().settings.set_string(crate::settings::MAIL_REPORT_PHISHING_ADDRESS, entry.text().trim());
        });
    }

    // Config → Mail → Junk email → "Safe senders…": opens the manage dialog
    // for the external-content trust flow (add/remove sender entries, change
    // trust levels; item 2.9 / M-17 reuses this list as the safe list). Every
    // change writes through to the UI-state database and the in-memory
    // `trusted_senders` mirror, so the reading pane's load policy picks it up
    // on the next decision.
    {
        let state = state.clone();
        let row = config_view.trusted_senders_row.clone();
        row.clone().connect_activated(move |_| {
            crate::trusted_senders::show_manage_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → Junk email → "Blocked senders…": opens the manage dialog
    // for the blocked-sender list (item 2.9 / M-17). New mail from a blocked
    // sender is moved to Junk on arrival by `rules::enforce_blocked_senders`.
    {
        let state = state.clone();
        let row = config_view.blocked_senders_row.clone();
        row.clone().connect_activated(move |_| {
            crate::blocked_senders::show_manage_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → Junk email → "Ignored conversations…": opens the manage
    // dialog for ignored conversations (item 2.8 / M-20). New mail in an
    // ignored conversation is moved to Deleted on arrival by
    // `rules::enforce_ignored_threads`.
    {
        let state = state.clone();
        let row = config_view.ignored_conversations_row.clone();
        row.clone().connect_activated(move |_| {
            crate::ignored_conversations::show_manage_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → "Search Folders…": manages the saved searches shown as
    // virtual folders in the folder pane (item 2.4 / M-19).
    {
        let state = state.clone();
        let row = config_view.search_folders_row.clone();
        row.clone().connect_activated(move |_| {
            crate::search_folders::show_manage_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → "Categories…": opens the shared category master-list
    // editor (item 2.1 / M-09, CAL-07, T-08). The same editor is reachable
    // from the Categorize menu's "Manage categories…" row.
    {
        let state = state.clone();
        let tags = tags.clone();
        let message_list = message_list.clone();
        let row = config_view.categories_row.clone();
        row.clone().connect_activated(move |_| {
            show_manage_categories_dialog(row.upcast_ref::<gtk::Widget>(), state.clone(), tags.clone(), message_list.clone());
        });
    }

    // Config → Mail → "Rules…": opens the rules manager (item 2.5 / M-14).
    {
        let state = state.clone();
        let row = config_view.rules_row.clone();
        row.clone().connect_activated(move |_| {
            crate::rules_view::show_manage_rules_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → "Quick Steps…": opens the Quick Steps manager
    // (item 2.6 / M-15).
    {
        let state = state.clone();
        let row = config_view.quick_steps_row.clone();
        row.clone().connect_activated(move |_| {
            crate::quick_steps_view::show_manage_quick_steps_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → "Automatic replies…": opens the per-account
    // out-of-office settings (item 2.11 / M-24).
    {
        let state = state.clone();
        let row = config_view.automatic_replies_row.clone();
        row.clone().connect_activated(move |_| {
            crate::automatic_replies_view::show_automatic_replies_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → "Rich text": sets the default body mode for future
    // compose sessions. Read at composer-open time, so an already-open
    // composer is untouched.
    {
        let state = state.clone();
        config_view.rich_text_row.connect_active_notify(move |row| {
            state.borrow_mut().rich_text_default = row.is_active();
            state.borrow().settings.set_bool(crate::settings::MAIL_RICH_TEXT_DEFAULT, row.is_active());
        });
    }

    // Config → Mail → "Check spelling as you type": persists the setting and
    // applies it to the composer's shared WebKit context immediately, so a
    // change (or the F7 toggle) affects an already-open composer.
    {
        let state = state.clone();
        config_view.spell_check_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_SPELL_CHECK, row.is_active());
            let languages = state.borrow().settings.get_strv(crate::settings::MAIL_SPELL_CHECK_LANGUAGES);
            crate::compose::apply_spell_check(row.is_active(), &languages);
        });
    }

    // Config → Mail → "Spelling language": maps the combo's index to a WebKit
    // tag (the first entry, empty, means the system locale) and applies it
    // live.
    {
        let state = state.clone();
        config_view.spell_languages_row.connect_selected_notify(move |row| {
            let tag = crate::config_view::SPELL_LANGUAGE_TAGS.get(row.selected() as usize).copied().unwrap_or("");
            let languages = if tag.is_empty() { Vec::new() } else { vec![tag.to_string()] };
            state.borrow().settings.set_strv(crate::settings::MAIL_SPELL_CHECK_LANGUAGES, languages.clone());
            let enabled = state.borrow().settings.get_bool(crate::settings::MAIL_SPELL_CHECK);
            crate::compose::apply_spell_check(enabled, &languages);
        });
    }

    // Config → Mail → "Send read receipts automatically": the reading pane's
    // read-receipt policy. Read per message at display time, so a flip
    // affects the next message opened - no re-render needed.
    {
        let state = state.clone();
        config_view.read_receipts_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_SEND_READ_RECEIPTS, row.is_active());
        });
    }

    // Config → Mail → "Mark as read": which mode decides when opening a
    // message marks it read (item 1.8). Read fresh in the selection handler on
    // every navigation, so a change takes effect on the next message; any mark
    // queued under the old mode is dropped here.
    {
        let state = state.clone();
        let delay_row = config_view.mark_as_read_delay_row.clone();
        config_view.mark_as_read_row.connect_selected_notify(move |row| {
            let mode = crate::settings::mark_as_read_at(row.selected());
            state.borrow().settings.set_string(crate::settings::MARK_AS_READ_MODE, mode);
            delay_row.set_sensitive(mode == "after-delay");
            cancel_pending_mark_read(&state);
            state.borrow_mut().mark_read_on_leave = None;
        });
    }

    // Config → Mail → "Mark as read" → "After": seconds a message stays open
    // before it is marked read in "after-delay" mode.
    {
        let state = state.clone();
        config_view.mark_as_read_delay_row.connect_value_notify(move |row| {
            state.borrow().settings.set_int(crate::settings::MARK_AS_READ_DELAY, row.value().round() as i32);
        });
    }

    // Config → Mail → "Undo send": how long a send waits in the outbox with
    // Undo on offer (item 1.13). Read at each Send, so it applies to the next.
    {
        let state = state.clone();
        config_view.undo_send_row.connect_selected_notify(move |row| {
            state
                .borrow()
                .settings
                .set_int(crate::settings::MAIL_UNDO_SEND_SECONDS, crate::settings::undo_send_at(row.selected()));
        });
    }

    // Config → Mail → attachment reminder (item 1.19 / C-16): read at each
    // Send, so it applies to the next composer.
    {
        let state = state.clone();
        config_view.warn_no_attachment_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_WARN_NO_ATTACHMENT, row.is_active());
        });
    }

    // Config → Mail → "Mail notifications": gates the new-mail/send-failure
    // desktop notifications. Read straight off the setting at the point each
    // event fires (see `connect_account`), so nothing needs re-arming here.
    {
        let state = state.clone();
        config_view.mail_notifications_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED, row.is_active());
        });
    }

    // Config → Mail → "Dock badge": gates the Unity LauncherEntry unread
    // badge. Republishing on the flip (rather than waiting for the next
    // `FoldersUpdated`) shows the badge the moment it's enabled, and
    // `refresh_unread_indicators` sends zero - hiding the badge - the
    // moment it's disabled. The next `rebuild_folder_tree` keeps it in
    // sync after that.
    {
        let state = state.clone();
        config_view.dock_badge_row.connect_active_notify(move |_row| {
            refresh_unread_indicators(&state);
        });
    }

    // Config → Calendar → "Event alerts": gates the reminder loop. The loop
    // (see `reminders::spawn_reminder_loop`) reads the key on every tick, so
    // nothing needs re-arming here - disabling mid-session simply stops new
    // notifications (and stops marking alerts as shown, so re-enabling
    // re-fires anything still due).
    {
        let state = state.clone();
        config_view.calendar_alerts_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::CALENDAR_ALERTS_ENABLED, row.is_active());
        });
    }

    // Config → Calendar → the calendar preferences (item 0.6). Each row writes
    // its GSettings key, then `apply_calendar_prefs` pushes the new values
    // into every calendar surface at once (item 3.1) - no restart needed.
    let apply_calendar_prefs: Rc<dyn Fn()> = {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let sidebar_mini = calendar_sidebar.mini_calendar.clone();
        let mail_overview_mini = mail_calendar_overview.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let message_list = message_list.clone();
        Rc::new(move || {
            let prefs = state.borrow().settings.calendar_preferences();
            crate::calendar_tz::set_display_zone(crate::calendar_tz::DisplayZone::from_setting(prefs.primary_tz));
            calendar_view::apply_preferences(&calendar_main, &prefs);
            calendar_view::set_mini_week_start(&sidebar_mini, prefs.week_start);
            calendar_view::set_mini_week_start(&mail_overview_mini, prefs.week_start);
            refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
            // The mail list's "This week"/"Last week" groups follow the week
            // start too: regroup what's on screen.
            crate::message_list::set_week_start(prefs.week_start);
            let (key, descending) = current_sort(&state);
            message_list.repopulate(message_list.all_messages(), key, descending);
        })
    };
    {
        let state = state.clone();
        let apply_calendar_prefs = apply_calendar_prefs.clone();
        config_view.calendar_week_start_row.connect_selected_notify(move |row| {
            state.borrow().settings.set_int(crate::settings::CALENDAR_WEEK_START, row.selected() as i32);
            apply_calendar_prefs();
        });
    }
    {
        let state = state.clone();
        let apply_calendar_prefs = apply_calendar_prefs.clone();
        config_view.calendar_work_days_row.connect_selected_notify(move |row| {
            let days = crate::settings::work_day_preset_at(row.selected());
            state.borrow().settings.set_strv(crate::settings::CALENDAR_WORK_DAYS, days);
            apply_calendar_prefs();
        });
    }
    {
        let state = state.clone();
        let apply_calendar_prefs = apply_calendar_prefs.clone();
        config_view.calendar_work_start_row.connect_value_notify(move |row| {
            state.borrow().settings.set_int(crate::settings::CALENDAR_WORK_START, row.value() as i32);
            apply_calendar_prefs();
        });
    }
    {
        let state = state.clone();
        let apply_calendar_prefs = apply_calendar_prefs.clone();
        config_view.calendar_work_end_row.connect_value_notify(move |row| {
            state.borrow().settings.set_int(crate::settings::CALENDAR_WORK_END, row.value() as i32);
            apply_calendar_prefs();
        });
    }
    {
        let state = state.clone();
        let apply_calendar_prefs = apply_calendar_prefs.clone();
        config_view.calendar_primary_tz_row.connect_selected_notify(move |row| {
            let value = crate::config_view::time_zone_at(row.selected());
            state.borrow().settings.set_string(crate::settings::CALENDAR_PRIMARY_TZ, &value);
            apply_calendar_prefs();
        });
    }
    {
        let state = state.clone();
        let apply_calendar_prefs = apply_calendar_prefs.clone();
        config_view.calendar_secondary_tz_row.connect_selected_notify(move |row| {
            let value = crate::config_view::time_zone_at(row.selected());
            state.borrow().settings.set_string(crate::settings::CALENDAR_SECONDARY_TZ, &value);
            apply_calendar_prefs();
        });
    }
    {
        let state = state.clone();
        config_view.calendar_default_reminder_row.connect_selected_notify(move |row| {
            state
                .borrow()
                .settings
                .set_int(crate::settings::CALENDAR_DEFAULT_REMINDER, crate::settings::default_reminder_at(row.selected()));
        });
    }

    // Config → Calendar → EDS mirror (EDS_TODO.md E2.4/E2.5). Seeding the
    // rows at startup is guarded off (`eds_mirror_seeding`) so a stored "on"
    // doesn't toast before the availability check; the startup block below
    // starts the mirror explicitly once it has seeded them.
    let eds_mirror_seeding = Rc::new(Cell::new(true));
    {
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let config_view = config_view.clone();
        let toast_overlay = toast_overlay.clone();
        let details_row = config_view.eds_mirror_details_row.clone();
        let seeding = eds_mirror_seeding.clone();
        config_view.eds_mirror_calendar_row.connect_active_notify(move |row| {
            if seeding.get() {
                return;
            }
            let enabled = row.is_active();
            state.borrow().settings.set_bool(crate::settings::EDS_MIRROR_CALENDAR, enabled);
            let details = state.borrow().settings.get_bool(crate::settings::EDS_MIRROR_DETAILS);
            details_row.set_sensitive(enabled);
            let toast = toast_overlay.clone();
            let row = row.clone();
            let on_error: Rc<dyn Fn(String)> = Rc::new(move |message| {
                let title = glib::markup_escape_text(&format!("Couldn't update the calendar mirror: {message}"));
                toast.add_toast(adw::Toast::new(&title));
                row.set_active(false);
            });
            crate::eds_mirror::apply_enabled(&calendar_state, &worker, enabled, details, on_error);
        });
    }
    {
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let seeding = eds_mirror_seeding.clone();
        config_view.eds_mirror_details_row.connect_active_notify(move |row| {
            if seeding.get() {
                return;
            }
            let details = row.is_active();
            state.borrow().settings.set_bool(crate::settings::EDS_MIRROR_DETAILS, details);
            crate::eds_mirror::set_details(&calendar_state, &worker, details);
        });
    }
    // Config → People → "File contacts as" (P-07): persists the preference and
    // repaints the People list so a re-file is visible immediately.
    {
        let state = state.clone();
        config_view.contacts_file_as_row.connect_selected_notify(move |row| {
            let value = crate::settings::contact_file_as_at(row.selected());
            state.borrow().settings.set_string(crate::settings::CONTACTS_FILE_AS, value);
            let refresh = state.borrow().contacts_list_refresh.clone();
            if let Some(refresh) = refresh {
                refresh();
            }
        });
    }
    // Config → People → "Include desktop address books" (EDS_TODO.md E1.4,
    // E1.5): persists the choice and flips the directory backend's switch,
    // which every lookup reads, so composers already open follow it, and
    // refills or clears the People screen's Desktop section. Turning it off
    // also closes the desktop's books and lets go of EDS.
    {
        let state = state.clone();
        let worker = worker.clone();
        config_view.eds_address_books_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            state.borrow().settings.set_bool(crate::settings::EDS_ADDRESS_BOOKS, enabled);
            crate::eds_directory::set_enabled(enabled);
            if !enabled {
                worker.spawn(crate::eds_directory::disconnect_desktop());
            }
            // Fill (or, switched off, clear) the People screen's Desktop section.
            crate::desktop_contacts::reload(&state, &worker);
        });
    }

    // Config → General → "Start Lookout at login": registers login
    // autostart with the session's Background portal when available (the
    // shell decides, first time via a dialog, and manages the registration
    // from then on - see the row's subtitle), falling back to a managed
    // XDG autostart file when there's no portal. Disabling removes the
    // XDG entry; a portal registration is revoked from the desktop's app
    // settings, since the portal API has no unregister call.
    {
        let state = state.clone();
        let worker = worker.clone();
        config_view.start_at_login_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            state.borrow().settings.set_bool(crate::settings::START_AT_LOGIN, enabled);
            if enabled {
                worker.spawn(async {
                    let _ = crate::background::enable_login_autostart().await;
                });
            } else {
                crate::background::disable_login_autostart();
            }
        });
    }

    // Config → General → "Keep running when the window is closed": read by
    // the main window's close-request handler at close time (see the
    // `connect_close_request` above), so nothing needs re-arming here.
    {
        let state = state.clone();
        config_view.close_to_background_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::CLOSE_TO_BACKGROUND, row.is_active());
        });
    }

    // Config → General → "Tray icon": starts/stops the StatusNotifierItem
    // (AppIndicator) service. Enabling registers the icon with the session's
    // watcher (absent watcher = the icon simply never appears; ksni retries);
    // disabling unregisters it, so the host drops the icon. Registration is
    // awaited off the main loop; once it lands, `refresh_unread_indicators`
    // paints the current count into the icon (a `stop` in the meantime makes
    // `start` discard the late service and resolve to `false`).
    // Tray commands cross back from the service thread on `tray_commands`.
    let (tray_commands, tray_commands_rx) = async_channel::unbounded();
    {
        let state = state.clone();
        config_view.tray_icon_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            state.borrow().settings.set_bool(crate::settings::TRAY_ICON_ENABLED, enabled);
            if enabled {
                let state = state.clone();
                let tray_commands = tray_commands.clone();
                glib::spawn_future_local(async move {
                    if crate::tray::start(tray_commands).await {
                        refresh_unread_indicators(&state);
                    } else {
                        tracing::debug!("tray service did not start (no StatusNotifierWatcher on the session bus, or switched off meanwhile?)");
                    }
                });
            } else {
                crate::tray::stop();
                refresh_unread_indicators(&state);
            }
        });
    }

    // Config → Advanced → "Aggressive prefetch": writes every control through
    // to GSettings, re-sends the assembled `PrefetchPolicy` to all account
    // sessions (applied live on each session's next main-loop iteration), and
    // greys the frequency/limit rows while the switch is off - "off" always
    // means the cooperative default, whatever the rows hold. Seeding below
    // fires these handlers too, which is what pushes the stored policy to the
    // sessions at startup.
    {
        let send_policy = {
            let state = state.clone();
            move || {
                let policy = state.borrow().settings.prefetch_policy();
                for handle in state.borrow().accounts.values() {
                    let _ = handle.cmd_tx.send_blocking(lookout_mail::session::AccountCommand::SetPrefetchPolicy(policy));
                }
            }
        };
        let interval_row = config_view.prefetch_interval_row.clone();
        let limit_row = config_view.prefetch_limit_row.clone();
        let batch_row = config_view.prefetch_batch_row.clone();
        let refresh_row = config_view.prefetch_refresh_row.clone();
        {
            let state = state.clone();
            let send_policy = send_policy.clone();
            let interval_row = interval_row.clone();
            let limit_row = limit_row.clone();
            let batch_row = batch_row.clone();
            let refresh_row = refresh_row.clone();
            config_view.aggressive_prefetch_row.connect_active_notify(move |row| {
                state.borrow().settings.set_bool(crate::settings::PREFETCH_AGGRESSIVE, row.is_active());
                let enabled = row.is_active();
                interval_row.set_sensitive(enabled);
                limit_row.set_sensitive(enabled);
                batch_row.set_sensitive(enabled);
                refresh_row.set_sensitive(enabled);
                send_policy();
            });
        }
        {
            let state = state.clone();
            let send_policy = send_policy.clone();
            interval_row.connect_value_notify(move |row| {
                state.borrow().settings.set_int(crate::settings::PREFETCH_BATCH_INTERVAL_SECONDS, row.value() as i32);
                send_policy();
            });
        }
        {
            let state = state.clone();
            let send_policy = send_policy.clone();
            limit_row.connect_value_notify(move |row| {
                state.borrow().settings.set_int(crate::settings::PREFETCH_FOLDER_LIMIT, row.value() as i32);
                send_policy();
            });
        }
        {
            let state = state.clone();
            let send_policy = send_policy.clone();
            batch_row.connect_value_notify(move |row| {
                state.borrow().settings.set_int(crate::settings::PREFETCH_BATCH_SIZE, row.value() as i32);
                send_policy();
            });
        }
        {
            let state = state.clone();
            let send_policy = send_policy;
            refresh_row.connect_value_notify(move |row| {
                state.borrow().settings.set_int(crate::settings::PREFETCH_REFRESH_INTERVAL_MINUTES, row.value() as i32);
                send_policy();
            });
        }
    }

    // The title bar's privacy toggle drives the tri-state (`privacy::BlurMode`)
    // and is the only control for it. `apply_blur_mode` stores the mode,
    // reloads the display-level blur provider, and refreshes the toggle's
    // checked state, icon and tooltip. The `suppress` flag stops the
    // programmatic `set_active` below from being mistaken for a user click
    // (which would otherwise advance the cycle again). Seeding further down
    // runs it once at startup.
    let suppress_blur_toggle = Rc::new(Cell::new(false));
    let apply_blur_mode: Rc<dyn Fn(crate::privacy::BlurMode)> = {
        let state = state.clone();
        let privacy_mode = privacy_mode.clone();
        let header_privacy_toggle = header_privacy_toggle.clone();
        let suppress_blur_toggle = suppress_blur_toggle.clone();
        Rc::new(move |mode| {
            state.borrow().settings.set_string(crate::settings::PRIVACY_BLUR_MODE, mode.as_setting());
            privacy_mode.set_mode(mode);
            suppress_blur_toggle.set(true);
            header_privacy_toggle.set_active(mode != crate::privacy::BlurMode::Off);
            suppress_blur_toggle.set(false);
            // The icon says which level is active; the tooltip names what the
            // next click does (the same convention the theme button uses).
            let (icon, fallback, tooltip) = match mode {
                crate::privacy::BlurMode::Off => ("view-reveal-symbolic", "view-conceal-symbolic", "Blur sensitive information"),
                crate::privacy::BlurMode::Sensitive => ("view-conceal-symbolic", "security-high-symbolic", "Blur all text"),
                crate::privacy::BlurMode::All => ("channel-secure-symbolic", "security-high-symbolic", "Show text"),
            };
            header_privacy_toggle.set_icon_name(themed_icon_name(&[icon, fallback]));
            header_privacy_toggle.set_tooltip_text(Some(tooltip));
        })
    };
    {
        // The header toggle: each click advances the cycle, read from the
        // stored mode (the single source of truth) so a programmatic
        // `set_active` can't shift it.
        let state = state.clone();
        let apply_blur_mode = apply_blur_mode.clone();
        header_privacy_toggle.connect_toggled(move |_btn| {
            if suppress_blur_toggle.get() {
                return;
            }
            let current = crate::privacy::BlurMode::from_setting(&state.borrow().settings.get_string(crate::settings::PRIVACY_BLUR_MODE));
            apply_blur_mode(current.next());
        });
    }

    // The tray's menu/click actions all target widgets this window owns, so
    {
        let app = app.clone();
        let state = state.clone();
        let window = window.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        let message_list = message_list.clone();
        glib::spawn_future_local(async move {
            while let Ok(command) = tray_commands_rx.recv().await {
                match command {
                    crate::tray::TrayCommand::ToggleWindow => {
                        if window.is_visible() {
                            window.set_visible(false);
                        } else {
                            window.present();
                        }
                    }
                    crate::tray::TrayCommand::OpenWindow => window.present(),
                    crate::tray::TrayCommand::Compose => compose_new_message(&state, &worker, &reading_stack, &message_list),
                    crate::tray::TrayCommand::Quit => app.quit(),
                }
            }
        });
    }
    // Phase 5: apply the persisted Config → Appearance/Mail switch states now
    // that their handlers are wired. Each `set_active` fires the notify
    // handler above, which re-derives UiState and any widget effect from the
    // new value, so startup can't drift from what was saved.
    {
        let persisted = state.borrow().settings.clone();
        config_view.animations_row.set_active(persisted.get_bool(crate::settings::ANIMATE_TRANSITIONS));
        config_view.message_theme_dark_row.set_active(persisted.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK));
        config_view.remote_images_row.set_active(persisted.get_bool(crate::settings::MAIL_LOAD_REMOTE_IMAGES));
        config_view
            .internal_domains_entry
            .set_text(&crate::settings::format_domain_list(&persisted.get_strv(crate::settings::MAIL_INTERNAL_DOMAINS)));
        config_view
            .report_phishing_entry
            .set_text(&persisted.get_string(crate::settings::MAIL_REPORT_PHISHING_ADDRESS));
        config_view.rich_text_row.set_active(persisted.get_bool(crate::settings::MAIL_RICH_TEXT_DEFAULT));
        config_view.spell_check_row.set_active(persisted.get_bool(crate::settings::MAIL_SPELL_CHECK));
        {
            let spell_langs = persisted.get_strv(crate::settings::MAIL_SPELL_CHECK_LANGUAGES);
            let index = spell_langs
                .first()
                .and_then(|tag| crate::config_view::SPELL_LANGUAGE_TAGS.iter().position(|t| t.eq_ignore_ascii_case(tag)))
                .unwrap_or(0);
            config_view.spell_languages_row.set_selected(index as u32);
        }
        config_view.read_receipts_row.set_active(persisted.get_bool(crate::settings::MAIL_SEND_READ_RECEIPTS));
        {
            let mark_mode = persisted.get_string(crate::settings::MARK_AS_READ_MODE);
            config_view.mark_as_read_row.set_selected(crate::settings::mark_as_read_index(&mark_mode));
            config_view.mark_as_read_delay_row.set_sensitive(mark_mode == "after-delay");
            config_view
                .mark_as_read_delay_row
                .set_value(persisted.get_int(crate::settings::MARK_AS_READ_DELAY).clamp(1, 60) as f64);
        }
        config_view
            .undo_send_row
            .set_selected(crate::settings::undo_send_index(persisted.get_int(crate::settings::MAIL_UNDO_SEND_SECONDS)));
        config_view.warn_no_attachment_row.set_active(persisted.get_bool(crate::settings::MAIL_WARN_NO_ATTACHMENT));
        config_view
            .mail_notifications_row
            .set_active(persisted.get_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED));
        config_view.dock_badge_row.set_active(persisted.get_bool(crate::settings::DOCK_BADGE_ENABLED));
        config_view.tray_icon_row.set_active(persisted.get_bool(crate::settings::TRAY_ICON_ENABLED));
        config_view.calendar_alerts_row.set_active(persisted.get_bool(crate::settings::CALENDAR_ALERTS_ENABLED));
        config_view
            .calendar_week_start_row
            .set_selected(persisted.get_int(crate::settings::CALENDAR_WEEK_START).clamp(0, 6) as u32);
        config_view
            .calendar_work_days_row
            .set_selected(crate::settings::work_day_preset_index(&persisted.get_strv(crate::settings::CALENDAR_WORK_DAYS)));
        config_view
            .calendar_work_start_row
            .set_value(persisted.get_int(crate::settings::CALENDAR_WORK_START).clamp(0, 23) as f64);
        config_view
            .calendar_work_end_row
            .set_value(persisted.get_int(crate::settings::CALENDAR_WORK_END).clamp(1, 24) as f64);
        config_view
            .calendar_primary_tz_row
            .set_selected(crate::config_view::time_zone_index(&persisted.get_string(crate::settings::CALENDAR_PRIMARY_TZ)));
        config_view
            .calendar_secondary_tz_row
            .set_selected(crate::config_view::time_zone_index(&persisted.get_string(crate::settings::CALENDAR_SECONDARY_TZ)));
        config_view
            .calendar_default_reminder_row
            .set_selected(crate::settings::default_reminder_index(persisted.get_int(crate::settings::CALENDAR_DEFAULT_REMINDER)));
        // Calendar → EDS mirror (E2.4/E2.5): seed the switches while the
        // handlers are suppressed, then start the mirror if it was on - which
        // re-creates the EDS source if it went missing and refreshes the file.
        let mirror_enabled = persisted.get_bool(crate::settings::EDS_MIRROR_CALENDAR);
        let mirror_details = persisted.get_bool(crate::settings::EDS_MIRROR_DETAILS);
        config_view.eds_mirror_details_row.set_active(mirror_details);
        config_view.eds_mirror_calendar_row.set_active(mirror_enabled);
        config_view.eds_mirror_details_row.set_sensitive(mirror_enabled);
        eds_mirror_seeding.set(false);
        if mirror_enabled {
            crate::eds_mirror::apply_enabled(
                &calendar_state,
                &worker,
                true,
                mirror_details,
                Rc::new(|e| tracing::warn!("EDS mirror couldn't start: {e}")),
            );
        }
        {
            let (tx, rx) = async_channel::bounded(1);
            worker.spawn(async move {
                let _ = tx.send(crate::eds_mirror::mirror_available().await).await;
            });
            let row = config_view.eds_mirror_calendar_row.clone();
            let details = config_view.eds_mirror_details_row.clone();
            glib::spawn_future_local(async move {
                if let Ok(false) = rx.recv().await {
                    row.set_subtitle(crate::config_view::EDS_MIRROR_UNAVAILABLE_SUBTITLE);
                    if !row.is_active() {
                        row.set_sensitive(false);
                        details.set_sensitive(false);
                    }
                }
            });
        }
        config_view
            .contacts_file_as_row
            .set_selected(crate::settings::contact_file_as_index(&persisted.get_string(crate::settings::CONTACTS_FILE_AS)));
        // People → "Include desktop address books": seed the switch, and the
        // backend explicitly too - the notify handler doesn't fire when the
        // stored value matches the switch's initial state. Then ask, off the
        // GTK thread, whether the desktop has address books Lookout can reach
        // at all (never starting EDS), and grey the switch out if not.
        let eds_enabled = persisted.get_bool(crate::settings::EDS_ADDRESS_BOOKS);
        config_view.eds_address_books_row.set_active(eds_enabled);
        crate::eds_directory::set_enabled(eds_enabled);
        {
            let (tx, rx) = async_channel::bounded(1);
            worker.spawn(async move {
                let _ = tx.send(crate::eds_directory::desktop_available().await).await;
            });
            let row = config_view.eds_address_books_row.clone();
            glib::spawn_future_local(async move {
                if let Ok(false) = rx.recv().await {
                    row.set_sensitive(false);
                    row.set_subtitle(crate::config_view::EDS_ADDRESS_BOOKS_UNAVAILABLE_SUBTITLE);
                }
            });
        }
        // "Start Lookout at login": the setting is the source of truth for
        // the portal path; the managed XDG file is the fallback, so an
        // entry left over (or written while the portal was absent) counts
        // as enabled too. Seeding fires the notify handler above, which for
        // an enabled start re-asserts the registration - a silent no-op
        // when the portal already knows.
        if crate::background::autostart_file_exists() && !persisted.get_bool(crate::settings::START_AT_LOGIN) {
            persisted.set_bool(crate::settings::START_AT_LOGIN, true);
        }
        config_view.start_at_login_row.set_active(persisted.get_bool(crate::settings::START_AT_LOGIN));
        config_view.close_to_background_row.set_active(persisted.get_bool(crate::settings::CLOSE_TO_BACKGROUND));
        // Theme rows: seeding fires the notify handlers above, which apply
        // the persisted theme/accent through the ThemeManager, so startup
        // can't drift from what was saved.
        let theme_id = persisted.get_string(crate::theme::THEME_ID_KEY);
        config_view.theme_row.set_selected(crate::theme::theme_index(&theme_id));
        let accent = persisted.get_string(crate::theme::ACCENT_COLOR_KEY);
        if let Some(rgba) = crate::theme::accent_rgba(&accent) {
            config_view.accent_color_button.set_rgba(&rgba);
        }
        config_view.accent_switch_row.set_active(!accent.is_empty());
        // Appearance → "Density" needs no seed here: its shared apply path
        // (View ▸ Layout ▸ Density and the ComboRow, wired above) seeds both
        // controls and the panes' `spacing-*` class from the stored value when
        // the window is built.
        // Appearance → "Message preview" and "Show sender images": seed from
        // the stored values (their notify handlers are no-ops for a value that
        // already matches, so nothing is written back on startup).
        config_view
            .preview_lines_row
            .set_selected(crate::settings::preview_lines(persisted.get_int(crate::settings::LAYOUT_PREVIEW_LINES)) as u32);
        config_view.show_sender_images_row.set_active(persisted.get_bool(crate::settings::MAIL_SHOW_SENDER_IMAGES));
        // Advanced → "Aggressive prefetch": seeding fires the notify handlers
        // above - the switch's handler re-applies the rows' sensitivity and
        // every handler re-sends the assembled `PrefetchPolicy` to the
        // account sessions, so startup always pushes the stored policy (and a
        // freshly-discovered account gets it on its first `FoldersUpdated`).
        config_view.aggressive_prefetch_row.set_active(persisted.get_bool(crate::settings::PREFETCH_AGGRESSIVE));
        config_view
            .prefetch_interval_row
            .set_value(persisted.get_int(crate::settings::PREFETCH_BATCH_INTERVAL_SECONDS) as f64);
        config_view.prefetch_limit_row.set_value(persisted.get_int(crate::settings::PREFETCH_FOLDER_LIMIT) as f64);
        config_view.prefetch_batch_row.set_value(persisted.get_int(crate::settings::PREFETCH_BATCH_SIZE) as f64);
        config_view
            .prefetch_refresh_row
            .set_value(persisted.get_int(crate::settings::PREFETCH_REFRESH_INTERVAL_MINUTES) as f64);
        // Advanced → "Privacy blur": apply the stored mode once, which loads
        // the blur provider and syncs the ComboRow and the header toggle.
        apply_blur_mode(crate::privacy::BlurMode::from_setting(&persisted.get_string(crate::settings::PRIVACY_BLUR_MODE)));
    }

    {
        let add_account_row = config_view.add_account_row.clone();
        let worker = worker.clone();
        add_account_row.connect_activated(move |_| {
            worker.spawn(crate::online_accounts::open_online_accounts());
        });
    }

    // Config → Appearance → "Window background": reflect a stored custom
    // background (if one applied at startup) in the row subtitle and arm the
    // restore row, seed the (always-enabled, wallpaper permitting) dimming
    // slider with the stored brightness regardless of which background is in
    // use, then wire the picker to a file chooser, the dimming slider into
    // the background's brightness, and "Restore default background" back to
    // the bundled artwork (which also resets dimming to its own default).
    let apply_background_brightness = {
        let background_dim = background_dim.clone();
        move |brightness: f64| background_dim.set_opacity(1.0 - brightness.clamp(0.0, 1.0))
    };
    // Tracks whether a custom background is currently in use, independent of
    // the "Background wallpaper" switch below - `apply_wallpaper_enabled`
    // reads it to decide "Restore default background"'s sensitivity on
    // re-enable, since that can't be inferred from the switch state alone.
    let has_custom_background = Rc::new(Cell::new(custom_background_name.is_some()));
    // Config → Appearance → "Background wallpaper": off hides the wallpaper
    // (and its dimming overlay) entirely, falling back to the window's own
    // plain theme background, and greys out the three rows below it - none
    // of them mean anything without a wallpaper to apply to.
    let apply_wallpaper_enabled = {
        let background = background.clone();
        let background_dim = background_dim.clone();
        let background_image_row = config_view.background_image_row.clone();
        let background_brightness_row = config_view.background_brightness_row.clone();
        let restore_background_row = config_view.restore_background_row.clone();
        let has_custom_background = has_custom_background.clone();
        move |enabled: bool| {
            background.set_visible(enabled);
            background_dim.set_visible(enabled);
            background_image_row.set_sensitive(enabled);
            background_brightness_row.set_sensitive(enabled);
            restore_background_row.set_sensitive(enabled && has_custom_background.get());
        }
    };
    {
        let brightness = settings.get_double(crate::settings::BACKGROUND_BRIGHTNESS);
        config_view.background_brightness_scale.set_value(brightness);
        apply_background_brightness(brightness);
    }
    if let Some(name) = &custom_background_name {
        config_view.background_image_row.set_subtitle(name);
        config_view.restore_background_row.set_sensitive(true);
    }
    {
        let wallpaper_enabled = settings.get_bool(crate::settings::BACKGROUND_WALLPAPER_ENABLED);
        config_view.wallpaper_enabled_row.set_active(wallpaper_enabled);
        apply_wallpaper_enabled(wallpaper_enabled);
    }
    {
        let background_image_row = config_view.background_image_row.clone();
        let restore_background_row = config_view.restore_background_row.clone();
        let background_brightness_scale = config_view.background_brightness_scale.clone();
        let background = background.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let settings = settings.clone();
        let has_custom_background = has_custom_background.clone();
        background_image_row.connect_activated(move |row| {
            let row = row.clone();
            let window = window.clone();
            let background = background.clone();
            let toast_overlay = toast_overlay.clone();
            let restore_background_row = restore_background_row.clone();
            let background_brightness_scale = background_brightness_scale.clone();
            let settings = settings.clone();
            let has_custom_background = has_custom_background.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_pixbuf_formats();
                filter.set_name(Some("Images"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);

                let dialog = gtk::FileDialog::builder().title("Choose a background image").filters(&filters).build();
                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                match gtk::gdk::Texture::from_filename(&path) {
                    Ok(texture) => {
                        background.set_paintable(Some(&texture));
                        crate::background_image::save(&settings, &path);
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
                        row.set_subtitle(&name);
                        restore_background_row.set_sensitive(true);
                        has_custom_background.set(true);
                        // Every freshly-picked image starts at 50% brightness
                        // (the `value-changed` handler persists it and applies
                        // it to the overlay).
                        background_brightness_scale.set_value(0.5);
                    }
                    Err(e) => {
                        let title = glib::markup_escape_text(&format!("Couldn't load background image: {e}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                }
            });
        });
    }
    {
        let background_brightness_scale = config_view.background_brightness_scale.clone();
        let settings = settings.clone();
        let apply_background_brightness = apply_background_brightness.clone();
        background_brightness_scale.connect_value_changed(move |scale| {
            let brightness = scale.value();
            settings.set_double(crate::settings::BACKGROUND_BRIGHTNESS, brightness);
            apply_background_brightness(brightness);
        });
    }
    {
        let restore_background_row = config_view.restore_background_row.clone();
        let background_image_row = config_view.background_image_row.clone();
        let background_brightness_scale = config_view.background_brightness_scale.clone();
        let background = background.clone();
        let settings = settings.clone();
        let apply_background_brightness = apply_background_brightness.clone();
        let has_custom_background = has_custom_background.clone();
        restore_background_row.connect_activated(move |row| {
            crate::background_image::clear(&settings);
            background.set_paintable(Some(&default_bg_texture));
            background_image_row.set_subtitle("Default Lookout artwork");
            // The bundled artwork defaults to 75% brightness: reset the
            // stored dim value so the slider reflects it too.
            settings.set_double(crate::settings::BACKGROUND_BRIGHTNESS, 0.75);
            background_brightness_scale.set_value(0.75);
            apply_background_brightness(0.75);
            row.set_sensitive(false);
            has_custom_background.set(false);
        });
    }
    {
        let settings = settings.clone();
        let apply_wallpaper_enabled = apply_wallpaper_enabled.clone();
        config_view.wallpaper_enabled_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            settings.set_bool(crate::settings::BACKGROUND_WALLPAPER_ENABLED, enabled);
            apply_wallpaper_enabled(enabled);
        });
    }

    // --- Global keyboard shortcuts. One window-level `EventControllerKey`
    // matches every key press against `shortcuts.rs`'s physical-keycode
    // table - the logical GSettings accelerators are resolved against the
    // keymap at startup, so a chord fires from anywhere in the window
    // (message list, reading pane, composer, search entry) and is
    // independent of the active keyboard layout. Actions dispatch through
    // the toolbar/rail buttons themselves (`emit_clicked` / `set_active`),
    // so each verb keeps exactly one implementation - its button handler,
    // including the sensitivity gating that makes a shortcut with no
    // selection a silent no-op (same convention as clicking the button).
    // The Config screen's Keyboard group captures replacements through the
    // same controller: while `capturing` is set the dispatcher is bypassed
    // and presses feed `set_chord` instead. ---
    let shortcut_settings = state.borrow().settings.clone();
    let shortcuts = Rc::new(RefCell::new(crate::shortcuts::ShortcutState::load(&shortcut_settings)));
    let display = gtk::gdk::Display::default();
    if let Some(display) = &display {
        shortcuts.borrow_mut().rebuild(display);
    }
    let mut dispatch: HashMap<&'static str, Rc<dyn Fn()>> = HashMap::new();
    for (action, button) in [
        (crate::shortcuts::ACTION_COMPOSE, &compose_button),
        (crate::shortcuts::ACTION_REPLY, &reply_button),
        (crate::shortcuts::ACTION_REPLY_ALL, &reply_all_button),
        (crate::shortcuts::ACTION_DELETE, &delete_button),
        (crate::shortcuts::ACTION_ARCHIVE, &archive_button),
        (crate::shortcuts::ACTION_SNOOZE, &snooze_button),
        (crate::shortcuts::ACTION_PIN, &pin_button),
        (crate::shortcuts::ACTION_MARK_READ, &mark_read_button),
    ] {
        let button = button.clone();
        dispatch.insert(action, Rc::new(move || button.emit_clicked()));
    }
    // Forward (its shortcut): the Forward split button's main action - the
    // plain `emit_clicked` the other buttons use.
    dispatch.insert(
        crate::shortcuts::ACTION_FORWARD,
        Rc::new({
            let forward_button = forward_button.clone();
            move || forward_button.emit_clicked()
        }),
    );
    // Report junk (its shortcut): the Report split button's main action.
    dispatch.insert(
        crate::shortcuts::ACTION_REPORT_JUNK,
        Rc::new({
            let report_button = report_button.clone();
            move || report_button.emit_clicked()
        }),
    );
    // Flag / clear flag (Insert, item 1.16): the Flag button's main action.
    // Insert also toggles overwrite in text fields, so it's left to them
    // while one has focus.
    dispatch.insert(
        crate::shortcuts::ACTION_FOLLOW_UP,
        Rc::new({
            let window = window.clone();
            let flag_button = flag_button.clone();
            move || {
                let typing = gtk::prelude::GtkWindowExt::focus(&window)
                    .is_some_and(|focus| focus.is::<gtk::Text>() || focus.is::<gtk::TextView>() || focus.downcast_ref::<webkit::WebView>().is_some_and(|view| view.is_editable()));
                if !typing {
                    flag_button.emit_clicked();
                }
            }
        }),
    );
    // Undo (Ctrl+Z): pop and replay the most recent reversible action
    // (item 0.4). A no-op when the stack is empty.
    dispatch.insert(
        crate::shortcuts::ACTION_UNDO,
        Rc::new({
            let state = state.clone();
            move || {
                undo_last(&state);
            }
        }),
    );
    // Quick Steps (Ctrl+Shift+1…9, item 2.6 / M-15): run the step in the
    // matching gallery slot on the current selection. A no-op when the slot is
    // empty or nothing is selected, same convention as the ribbon buttons.
    for (slot, action) in crate::shortcuts::ACTION_QUICK_STEP_SLOTS.iter().copied().enumerate() {
        let state = state.clone();
        let message_list = message_list.clone();
        dispatch.insert(action, Rc::new(move || crate::quick_steps::apply_quick_step_slot(&state, &message_list, slot)));
    }
    // Collapse / expand the ribbon (Ctrl+F1, UX-16): hides the command band
    // while the menu bar's File/Home/View/Help tab strip stays put, and
    // remembers the choice in `ribbon-collapsed`.
    {
        let settings = settings.clone();
        let icon_toolbar_box = icon_toolbar_box.clone();
        let collapsed = Rc::new(Cell::new(settings.get_bool(crate::settings::RIBBON_COLLAPSED)));
        icon_toolbar_box.set_visible(!collapsed.get());
        dispatch.insert(
            crate::shortcuts::ACTION_TOGGLE_RIBBON,
            Rc::new(move || {
                let next = !collapsed.get();
                collapsed.set(next);
                settings.set_bool(crate::settings::RIBBON_COLLAPSED, next);
                icon_toolbar_box.set_visible(!next);
            }),
        );
    }
    // Print: the reading-pane print path (the calendar toolbar's Print
    // button prints the month instead), gated the same way the More menu's
    // Print item is: only while a message is actually rendered.
    dispatch.insert(
        crate::shortcuts::ACTION_PRINT,
        Rc::new({
            let window = window.clone();
            let reading_stack = reading_stack.clone();
            let state = state.clone();
            move || {
                let parent: &gtk::Window = window.upcast_ref();
                print_reading_pane(&state, &reading_stack, parent);
            }
        }),
    );
    // Check spelling (F7): flips the composer's spellcheck setting and applies
    // it to the live composer context, confirming with a toast. WebKitGTK has
    // no spelling dialog, so this is a toggle rather than Outlook's modal.
    dispatch.insert(
        crate::shortcuts::ACTION_SPELL,
        Rc::new({
            let state = state.clone();
            move || {
                let settings = state.borrow().settings.clone();
                let enabled = !settings.get_bool(crate::settings::MAIL_SPELL_CHECK);
                settings.set_bool(crate::settings::MAIL_SPELL_CHECK, enabled);
                let languages = settings.get_strv(crate::settings::MAIL_SPELL_CHECK_LANGUAGES);
                crate::compose::apply_spell_check(enabled, &languages);
                if let Some(overlay) = state.borrow().toast_overlay.clone() {
                    overlay.add_toast(adw::Toast::new(if enabled { "Spelling checks on" } else { "Spelling checks off" }));
                }
            }
        }),
    );
    // Find: Ctrl+F now lives in the same table as everything else (it
    // replaced the old `ShortcutController` block above). Selecting the
    // entry's existing text means typing replaces the old query.
    dispatch.insert(
        crate::shortcuts::ACTION_SEARCH,
        Rc::new({
            let search_entry = search_entry.clone();
            move || {
                search_entry.select_region(0, -1);
                search_entry.grab_focus();
            }
        }),
    );
    // Close reading pane: back to the blank pane, mirroring the state reset
    // the message-selection handler performs for "nothing selected". A
    // composer in the pane is left alone - drafts are never yanked away.
    dispatch.insert(
        crate::shortcuts::ACTION_CLOSE_PANE,
        Rc::new({
            let state = state.clone();
            let reading_stack = reading_stack.clone();
            let web_view = web_view.clone();
            let user_content_manager = user_content_manager.clone();
            let theme_override_sheet = theme_override_sheet.clone();
            move || {
                if reading_stack.visible_child_name().as_deref() == Some("compose") {
                    return;
                }
                // The message-theme override resets to the configured default
                // and its physical sheet/canvas are re-armed to match, the
                // same as every other navigation path.
                let default_dark = {
                    let mut st = state.borrow_mut();
                    st.pending_body_request = None;
                    st.pending_html_reveal = false;
                    st.reveal_generation += 1;
                    st.pending_header = None;
                    st.pending_attachment = None;
                    st.pending_raw_message = None;
                    st.unsubscribe_info = None;
                    st.unsubscribe_dismissed = None;
                    st.imip = None;
                    st.imip_dismissed = None;
                    st.read_receipt_request = None;
                    st.read_receipt_dismissed = None;
                    st.read_receipt_context = None;
                    st.rendered_trust_sender = None;
                    st.load_once_images = false;
                    st.message_theme_override = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                    st.trust_banner_dismissed = None;
                    st.rendered_message = None;
                    st.rendered_draft = None;
                    st.rendered_snooze = None;
                    st.rendered_follow_up = None;
                    st.pending_draft_open = None;
                    st.message_theme_override
                };
                drop_pending_cid(&state);
                set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
                reading_stack.set_visible_child_name("empty");
            }
        }),
    );
    // Module switching via the rail toggles: `set_active` fires the same
    // `toggled` handler a click would, and is a no-op when already there.
    // (Settings is not a module anymore - its shortcut opens the modal
    // dialog via a `dispatch` entry inserted below, alongside `refresh_config`.)
    for (action, button) in [
        (crate::shortcuts::ACTION_MAIL, &mail_view_button),
        (crate::shortcuts::ACTION_CALENDAR, &calendar_view_button),
        (crate::shortcuts::ACTION_CONTACTS, &contacts_view_button),
        (crate::shortcuts::ACTION_TASKS, &tasks_view_button),
        (crate::shortcuts::ACTION_LOOKOUT, &lookout_view_button),
    ] {
        let button = button.clone();
        dispatch.insert(action, Rc::new(move || button.set_active(true)));
    }

    // The Config keyboard rows refresh through this closure: it repaints
    // every row's accelerator label from the live table.
    // UX-14: the rail's tooltips name each module's current shortcut, e.g.
    // "Calendar (Ctrl+2)". They refresh from the same closure as the Config
    // keyboard rows, so a rebound chord shows up there immediately.
    let nav_rail_tooltips: Vec<(&'static str, &'static str, gtk::ToggleButton)> = vec![
        (crate::shortcuts::ACTION_MAIL, "Mail", mail_view_button.clone()),
        (crate::shortcuts::ACTION_CALENDAR, "Calendar", calendar_view_button.clone()),
        (crate::shortcuts::ACTION_CONTACTS, "People", contacts_view_button.clone()),
        (crate::shortcuts::ACTION_TASKS, "Tasks", tasks_view_button.clone()),
        (crate::shortcuts::ACTION_LOOKOUT, "Lookout", lookout_view_button.clone()),
    ];

    let refresh_shortcut_rows: Rc<dyn Fn()> = Rc::new({
        let shortcuts = shortcuts.clone();
        let config_view = config_view.clone();
        let nav_rail_tooltips = nav_rail_tooltips.clone();
        move || {
            let table = shortcuts.borrow();
            for row in config_view.keyboard_rows.borrow().iter() {
                row.label.set_label(&table.accel_for(row.action));
            }
            for (action, label, button) in &nav_rail_tooltips {
                button.set_tooltip_text(Some(&table.tooltip_for(label, action)));
            }
        }
    });

    // The action currently being recorded by the Config rows, or None.
    let capture_action: Rc<RefCell<Option<&'static str>>> = Rc::new(RefCell::new(None));

    // Row wiring: activating a row starts (or re-targets) the recording;
    // activating the row that is recording again cancels it.
    {
        let rows: Vec<_> = config_view
            .keyboard_rows
            .borrow()
            .iter()
            .map(|row| (row.row.clone(), row.label.clone(), row.action))
            .collect();
        for (row, label, action) in rows {
            let shortcuts = shortcuts.clone();
            let capture_action = capture_action.clone();
            let refresh_shortcut_rows = refresh_shortcut_rows.clone();
            row.connect_activated(move |_| {
                let capturing = shortcuts.borrow().capturing;
                if capturing && *capture_action.borrow() == Some(action) {
                    shortcuts.borrow_mut().capturing = false;
                    *capture_action.borrow_mut() = None;
                    refresh_shortcut_rows();
                    return;
                }
                *capture_action.borrow_mut() = Some(action);
                shortcuts.borrow_mut().capturing = true;
                label.set_label("Press keys…");
            });
        }
    }

    // "Restore default shortcuts": back to the shipped bindings.
    {
        let shortcuts = shortcuts.clone();
        let shortcut_settings = shortcut_settings.clone();
        let display = display.clone();
        let refresh_shortcut_rows = refresh_shortcut_rows.clone();
        config_view.reset_shortcuts_row.connect_activated(move |_| {
            let Some(display) = &display else { return };
            shortcuts.borrow_mut().reset_all(&shortcut_settings, display);
            refresh_shortcut_rows();
        });
    }

    // The single controller: dispatch matching chords, or feed the capture
    // flow while a Config row is recording. Stop on anything handled, so a
    // matched chord never also reaches the focused widget (an entry
    // retyping "n" because Ctrl+N got through would be wrong).
    let dispatch = Rc::new(RefCell::new(dispatch));
    {
        let shortcuts = shortcuts.clone();
        let shortcut_settings = shortcut_settings.clone();
        let capture_action = capture_action.clone();
        let refresh_shortcut_rows = refresh_shortcut_rows.clone();
        let state = state.clone();
        let dispatch = dispatch.clone();
        let message_list = message_list.clone();
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(move |_controller, keyval, keycode, state_modifiers| {
            if shortcuts.borrow().capturing {
                if let Some(display) = &display {
                    if let Some((modifiers, keyval)) = crate::shortcuts::chord_from_key(display, keycode, state_modifiers) {
                        // Escape (no modifiers) cancels the recording.
                        if keyval == gtk::gdk::Key::Escape && (modifiers & crate::shortcuts::MODIFIER_MASK).is_empty() {
                            shortcuts.borrow_mut().capturing = false;
                            *capture_action.borrow_mut() = None;
                            refresh_shortcut_rows();
                            return glib::Propagation::Stop;
                        }
                    }
                }
                let Some(action) = *capture_action.borrow() else {
                    return glib::Propagation::Stop;
                };
                let Some(display) = &display else {
                    return glib::Propagation::Stop;
                };
                match shortcuts.borrow_mut().set_chord(&shortcut_settings, display, action, keycode, state_modifiers) {
                    Ok(_) => {
                        shortcuts.borrow_mut().capturing = false;
                        *capture_action.borrow_mut() = None;
                        refresh_shortcut_rows();
                    }
                    Err(err) => {
                        // Stay in recording mode; the toast says what to fix.
                        let st = state.borrow();
                        if let Some(overlay) = &st.toast_overlay {
                            overlay.add_toast(adw::Toast::new(&err.message()));
                        }
                    }
                }
                return glib::Propagation::Stop;
            }
            let Some(action) = shortcuts.borrow().action_for(keycode, state_modifiers) else {
                // Classic category shortcuts (item 2.1, Outlook keymap):
                // Ctrl+F2 … Ctrl+F12 apply the Nth category to the selected
                // messages. Checked after the registry so a user's own binding
                // always wins.
                if state_modifiers & crate::shortcuts::MODIFIER_MASK == gtk::gdk::ModifierType::CONTROL_MASK {
                    if let Some(n) = category_function_key(keyval) {
                        let key = state.borrow().categories.borrow().by_shortcut(n).map(|c| c.key.clone());
                        if let Some(key) = key {
                            let targets = message_list.selected_summaries();
                            if !targets.is_empty() {
                                send_keyword_store(&state, &targets, &lookout_core::tag_keyword(&key), true);
                                return glib::Propagation::Stop;
                            }
                        }
                    }
                }
                return glib::Propagation::Proceed;
            };
            if let Some(run) = dispatch.borrow().get(action) {
                run();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(controller);
    }

    // Seed the Config rows' labels once; captures refresh them live.
    refresh_shortcut_rows();

    // The config view's manage-identities dialog refreshes through this
    // slot: it's filled with `refresh_config` right after that closure is
    // built below (a dialog's `on_changed` can't capture the closure that
    // built its anchor row - that would be a self-reference).
    let refresh_hook: Rc<RefCell<Rc<dyn Fn()>>> = Rc::new(RefCell::new(Rc::new(|| {})));
    let refresh_config: Rc<dyn Fn()> = Rc::new({
        let refresh_hook = refresh_hook.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let config_view = config_view.clone();
        // Captured purely so the Edit/Remove closures built below (for
        // manual "other" accounts) can reconnect or tear down a session -
        // everything `connect_other_account` needs, mirroring the set
        // `spawn_account_discovery` already threads through to
        // `connect_account`.
        let worker = worker.clone();
        let keyring = keyring.clone();
        let window = window.clone();
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        let list_header = list_header.clone();
        let dashboard_refresh = dashboard_refresh.clone();
        let mark_read_icon = mark_read_icon.clone();
        let pin_icon = pin_icon.clone();
        // Everything the GOA enable/disable toggle needs beyond the mail
        // set above: the calendar/tasks/contacts wiring, and the page-state
        // cells the re-enable path flips back from their empty states.
        let root_stack = root_stack.clone();
        let current_mail_page = current_mail_page.clone();
        let mail_view_button = mail_view_button.clone();
        let current_calendar_page = current_calendar_page.clone();
        let calendar_view_button = calendar_view_button.clone();
        let current_tasks_page = current_tasks_page.clone();
        let tasks_view_button = tasks_view_button.clone();
        let current_lookout_page = current_lookout_page.clone();
        let lookout_view_button = lookout_view_button.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_sidebar.calendar_list_box.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let reminders_engine = reminders_engine.clone();
        let tasks_view = tasks_view.clone();
        let refresh_contacts_ui = refresh_contacts_ui.clone();
        move || {
            let st = state.borrow();
            // The GOA enable/disable list - every account the discoveries
            // saw, disabled ones included (a disabled account must stay
            // visible here so it can be turned back on).
            let mut goa: Vec<crate::config_view::GoaAccountInfo> = st
                .goa_accounts
                .iter()
                .map(|(account_id, a)| crate::config_view::GoaAccountInfo {
                    display_name: a.display_name.clone(),
                    email: a.email.clone(),
                    account_id: account_id.0.clone(),
                    enabled: st.account_enabled(account_id),
                })
                .collect();
            goa.sort_by_key(|a| a.display_name.to_lowercase());
            let mut mail: Vec<crate::config_view::MailAccountInfo> = st
                .accounts
                .iter()
                .map(|(account_id, h)| crate::config_view::MailAccountInfo {
                    display_name: h.display_name.clone(),
                    email: h.email.clone(),
                    account_id: account_id.0.clone(),
                    is_other: account_id.0.starts_with(crate::app_config::OTHER_ACCOUNT_ID_PREFIX),
                    identity_labels: st
                        .app_config
                        .borrow()
                        .identities_for_account(account_id, &h.display_name, &h.email)
                        .iter()
                        .map(|i| i.label())
                        .collect(),
                    imap: format!("{}:{}", h.imap_host, h.imap_port),
                    smtp: format!("{}:{}", h.smtp_host, h.smtp_port),
                    graph_pin_status: match &h.graph_pin {
                        None => crate::config_view::GraphPinStatus::NotApplicable,
                        Some(client) if client.consented() => crate::config_view::GraphPinStatus::Enabled,
                        Some(_) => crate::config_view::GraphPinStatus::NeedsConsent,
                    },
                    // `graph_pin` is Some for exactly the accounts that own a
                    // Microsoft 365 mailbox (never shared or manual ones), so
                    // it doubles as the transport's eligibility marker.
                    graph_mail: match (&h.graph_pin, h.uses_graph_mail) {
                        (Some(_), true) => crate::config_view::GraphMailStatus::On,
                        (Some(_), false) => crate::config_view::GraphMailStatus::Off,
                        (None, _) => crate::config_view::GraphMailStatus::NotApplicable,
                    },
                })
                .collect();
            let webcal_list = st.app_config.borrow().webcal_subscriptions.clone();
            let signatures_list = st.app_config.borrow().signatures.clone();
            let signature_defaults = st.app_config.borrow().signature_defaults.clone();
            let templates_list = st.app_config.borrow().templates.clone();
            let goa_unavailable = st.goa_unavailable;
            drop(st);
            mail.sort_by_key(|a| a.email.to_lowercase());
            let mut calendar: Vec<crate::config_view::CalendarAccountInfo> = calendar_state
                .borrow()
                .accounts
                .values()
                .map(|h| crate::config_view::CalendarAccountInfo {
                    display_name: h.display_name.clone(),
                    uri: h.uri.clone(),
                })
                .collect();
            calendar.sort_by_key(|a| a.display_name.to_lowercase());
            let mut webcal: Vec<crate::config_view::WebcalSubscriptionInfo> = webcal_list
                .iter()
                .map(|sub| crate::config_view::WebcalSubscriptionInfo {
                    display_name: sub.display_name.clone(),
                    url: sub.url.clone(),
                })
                .collect();
            webcal.sort_by_key(|a| a.display_name.to_lowercase());

            let (mail_cache_dir, mail_files) = lookout_mail::cache_info();
            let mail_cache_files: Vec<crate::config_view::CacheFile> = mail_files
                .into_iter()
                .map(|(name, size)| crate::config_view::CacheFile {
                    name,
                    size: crate::config_view::format_size(size),
                })
                .collect();
            let (calendar_cache_dir, cal_files) = lookout_dav::cache_info();
            let calendar_cache_files: Vec<crate::config_view::CacheFile> = cal_files
                .into_iter()
                .map(|(name, size)| crate::config_view::CacheFile {
                    name,
                    size: crate::config_view::format_size(size),
                })
                .collect();
            let (contacts_cache_dir, contacts_files) = lookout_dav::contacts_cache_info();
            let contacts_cache_files: Vec<crate::config_view::CacheFile> = contacts_files
                .into_iter()
                .map(|(name, size)| crate::config_view::CacheFile {
                    name,
                    size: crate::config_view::format_size(size),
                })
                .collect();

            let manage_identities: crate::config_view::ManageIdentities = {
                let state = state.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |anchor, account_id| {
                    let account_id = AccountId(account_id.to_string());
                    let app_config = state.borrow().app_config.clone();
                    let on_changed = {
                        let state = state.clone();
                        let refresh_hook = refresh_hook.borrow().clone();
                        Rc::new(move || {
                            // Any open composer's From dropdown re-reads its
                            // identities live, so a change made in Config
                            // while composing lands in the open list too.
                            if let Some(refresh) = state.borrow().composer_identities_refresh.clone() {
                                refresh();
                            }
                            refresh_hook();
                        })
                    };
                    crate::identities::show_manage_dialog(anchor, app_config, account_id, on_changed);
                })
            };
            // Config → Accounts → Signatures row actions: edit reopens the
            // signature editor prefilled with the saved content; remove
            // deletes the signature outright (no confirmation - signatures
            // are cheap to recreate, matching the identities dialog).
            let edit_signature: crate::config_view::SignaturesAction = {
                let state = state.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |anchor, id| {
                    let Some(signature) = state.borrow().app_config.borrow().signatures.iter().find(|s| &s.id == id).cloned() else {
                        return;
                    };
                    let app_config = state.borrow().app_config.clone();
                    let on_changed = {
                        let refresh_hook = refresh_hook.borrow().clone();
                        Rc::new(move || refresh_hook())
                    };
                    crate::signatures::show_editor_dialog(anchor, app_config, Some(signature), on_changed);
                })
            };
            let remove_signature: crate::config_view::SignaturesAction = {
                let state = state.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |_anchor, id| {
                    state.borrow().app_config.borrow_mut().remove_signature(id);
                    crate::app_config::save(&state.borrow().app_config.borrow());
                    let refresh = refresh_hook.borrow().clone();
                    refresh();
                })
            };
            // Config → Mail → My Templates row actions (item 2.10 / C-17):
            // edit reopens the template editor prefilled; remove deletes it.
            let edit_template: crate::config_view::TemplatesAction = {
                let state = state.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |anchor, id| {
                    let Some(template) = state.borrow().app_config.borrow().templates.iter().find(|t| &t.id == id).cloned() else {
                        return;
                    };
                    let app_config = state.borrow().app_config.clone();
                    let on_changed = {
                        let refresh_hook = refresh_hook.borrow().clone();
                        Rc::new(move || refresh_hook())
                    };
                    crate::templates::show_editor_dialog(anchor, app_config, Some(template), on_changed);
                })
            };
            let remove_template: crate::config_view::TemplatesAction = {
                let state = state.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |_anchor, id| {
                    state.borrow().app_config.borrow_mut().remove_template(id);
                    crate::app_config::save(&state.borrow().app_config.borrow());
                    let refresh = refresh_hook.borrow().clone();
                    refresh();
                })
            };
            // Config → Mail accounts → default-signature dropdowns: write the
            // chosen signature id (or `None` for no signature) through to
            // `app_config` for the account the dropdown belongs to,
            // immediately. The dropdown already shows the selection, so no
            // refresh is needed - the value just has to survive restarts
            // and be ready for the composer.
            let set_default_signature: crate::config_view::SetAccountSignatureDefault = {
                let state = state.clone();
                Rc::new(move |account_id, kind, id| {
                    let st = state.borrow();
                    let mut config = st.app_config.borrow_mut();
                    let defaults = config.signature_defaults.entry(AccountId(account_id.to_string())).or_default();
                    match kind {
                        crate::config_view::SignatureDefault::New => defaults.new = id,
                        crate::config_view::SignatureDefault::Reply => defaults.reply = id,
                    }
                    drop(config);
                    crate::app_config::save(&st.app_config.borrow());
                })
            };
            let edit_other: crate::config_view::OtherAccountAction = {
                let state = state.clone();
                let worker = worker.clone();
                let keyring = keyring.clone();
                let window = window.clone();
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_icon = mark_read_icon.clone();
                let pin_icon = pin_icon.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |anchor, account_id: &str| {
                    let Some(existing) = state.borrow().app_config.borrow().other_account(account_id) else {
                        return;
                    };
                    let app_config = state.borrow().app_config.clone();
                    let on_saved = {
                        let state = state.clone();
                        let worker = worker.clone();
                        let keyring = keyring.clone();
                        let window = window.clone();
                        let app = app.clone();
                        let toast_overlay = toast_overlay.clone();
                        let folder_selection = folder_selection.clone();
                        let folder_scroller = folder_scroller.clone();
                        let message_list = message_list.clone();
                        let message_list_stack = message_list_stack.clone();
                        let message_header = message_header.clone();
                        let reading_stack = reading_stack.clone();
                        let list_header = list_header.clone();
                        let dashboard_refresh = dashboard_refresh.clone();
                        let mark_read_icon = mark_read_icon.clone();
                        let pin_icon = pin_icon.clone();
                        let refresh_hook = refresh_hook.borrow().clone();
                        Rc::new(move |account_id: &str| {
                            // Editing always fully reconnects: the running
                            // session's `AccountConfig` is immutable once
                            // started, and a full reconnect is cheap and
                            // always correct, unlike diffing which fields
                            // actually changed. Dropping the old handle
                            // closes its command channel, which
                            // `run_account_session` treats as a clean
                            // shutdown request.
                            state.borrow_mut().accounts.remove(&AccountId(account_id.to_string()));
                            let Some(account) = state.borrow().app_config.borrow().other_account(account_id) else {
                                return;
                            };
                            connect_other_account(
                                worker.clone(),
                                state.clone(),
                                folder_selection.clone(),
                                folder_scroller.clone(),
                                message_list.clone(),
                                message_list_stack.clone(),
                                message_header.clone(),
                                reading_stack.clone(),
                                toast_overlay.clone(),
                                window.clone(),
                                app.clone(),
                                list_header.clone(),
                                account,
                                keyring.clone(),
                                dashboard_refresh.clone(),
                                mark_read_icon.clone(),
                                pin_icon.clone(),
                            );
                            refresh_hook();
                        })
                    };
                    crate::other_accounts::show_add_account_dialog(anchor, worker.clone(), app_config, keyring.clone(), Some(existing), None, on_saved);
                })
            };
            let remove_other: crate::config_view::OtherAccountAction = {
                let state = state.clone();
                let worker = worker.clone();
                let keyring = keyring.clone();
                let toast_overlay = toast_overlay.clone();
                let refresh_hook = refresh_hook.clone();
                let calendar_state = calendar_state.clone();
                Rc::new(move |_anchor, account_id: &str| {
                    let account_id = AccountId(account_id.to_string());
                    state.borrow_mut().accounts.remove(&account_id);
                    {
                        let app_config = state.borrow().app_config.clone();
                        app_config.borrow_mut().other_accounts.retain(|a| a.id != account_id.0);
                        crate::app_config::save(&app_config.borrow());
                    }
                    // The account is gone for good (unlike a Config disable,
                    // which keeps the on-disk state for a re-enable), so prune
                    // its folder-pane expansion rows too - the mailbox ids
                    // would otherwise linger forever in the UI-state DB.
                    if let Some(db) = state.borrow().ui_db.clone() {
                        if let Err(e) = db.remove_account_folder_expansion(&account_id) {
                            tracing::warn!("could not prune folder expansion for removed account {account_id}: {e}");
                        }
                    }
                    let keyring = keyring.clone();
                    let account_id_for_worker = account_id.clone();
                    worker.spawn(async move {
                        let _ = keyring.delete_account(&account_id_for_worker).await;
                    });
                    if let Err(e) = lookout_mail::remove_account_cache(&account_id) {
                        tracing::warn!("could not remove cache for removed account {account_id}: {e}");
                    }
                    toast_overlay.add_toast(adw::Toast::new("Account removed"));
                    // Its events must leave the desktop mirror now, not at the
                    // next calendar refresh (EDS_TODO.md E2.4).
                    crate::eds_mirror::request_sync(&calendar_state, &worker, None);
                    let refresh = refresh_hook.borrow().clone();
                    refresh();
                })
            };
            // Config's "Sync pins and flags with Outlook" action for a Microsoft 365
            // account that hasn't opted in yet - runs the interactive Graph
            // consent flow and, on success, re-runs this closure so the row
            // redraws without the button. Declining is simply not clicking
            // it: `\Flagged`-driven pinning is unaffected either way.
            let enable_graph_pin: crate::config_view::OtherAccountAction = {
                let state = state.clone();
                let toast_overlay = toast_overlay.clone();
                let refresh_hook = refresh_hook.clone();
                let message_list = message_list.clone();
                // The calendar wiring, so the account's Microsoft 365 calendar
                // can come online the moment Graph is consented (item 19) -
                // it couldn't start before then.
                let worker = worker.clone();
                let calendar_state = calendar_state.clone();
                let calendar_main = calendar_main.clone();
                let calendar_list_box = calendar_list_box.clone();
                let mini_calendar = mini_calendar.clone();
                let mail_overview_day = mail_overview_day.clone();
                let mail_overview_day_list = mail_overview_day_list.clone();
                let reminders_engine = reminders_engine.clone();
                let tasks_view = tasks_view.clone();
                let refresh_contacts_ui = refresh_contacts_ui.clone();
                Rc::new(move |_anchor, account_id: &str| {
                    let Some(client) = state.borrow().accounts.get(&AccountId(account_id.to_string())).and_then(|h| h.graph_pin.clone()) else {
                        return;
                    };
                    let toast_overlay = toast_overlay.clone();
                    let refresh_hook = refresh_hook.clone();
                    let state = state.clone();
                    let message_list = message_list.clone();
                    let worker = worker.clone();
                    let calendar_state = calendar_state.clone();
                    let calendar_main = calendar_main.clone();
                    let calendar_list_box = calendar_list_box.clone();
                    let mini_calendar = mini_calendar.clone();
                    let mail_overview_day = mail_overview_day.clone();
                    let mail_overview_day_list = mail_overview_day_list.clone();
                    let reminders_engine = reminders_engine.clone();
                    let tasks_view = tasks_view.clone();
                    let refresh_contacts_ui = refresh_contacts_ui.clone();
                    let account_id = AccountId(account_id.to_string());
                    glib::spawn_future_local(async move {
                        match client.ensure_consent().await {
                            Ok(()) => {
                                toast_overlay.add_toast(adw::Toast::new("Outlook sync enabled"));
                                // The same grant covers follow-up flags (item
                                // 1.16) and the category master list (item
                                // 2.1): sync both with Outlook now.
                                sync_mail_properties_from_graph(&state, &account_id);
                                sync_categories_from_graph(&state, &message_list, &account_id);
                                push_categories_to_graph(&state);
                                sync_and_push_rules_from_graph(&state, &account_id);
                                // Bring the account's Microsoft 365 calendar
                                // online now that Graph is consented (item 19).
                                let (goa_client, goa_calendar, contacts_account) = {
                                    let st = state.borrow();
                                    let entry = st.goa_accounts.get(&account_id);
                                    (
                                        st.goa_client.clone(),
                                        entry.and_then(|a| a.calendar.clone()),
                                        entry.and_then(|a| a.contacts.clone().or_else(|| microsoft_graph_contacts_account(a, &account_id))),
                                    )
                                };
                                if let (Some(goa_client), Some(goa_account)) = (goa_client.clone(), goa_calendar) {
                                    let shared_owners = crate::window::connect::shared_owners_for(&state, &account_id);
                                    connect_calendar_account(
                                        worker.clone(),
                                        calendar_state.clone(),
                                        calendar_main.clone(),
                                        calendar_list_box.clone(),
                                        mini_calendar.clone(),
                                        mail_overview_day.clone(),
                                        mail_overview_day_list.clone(),
                                        reminders_engine.clone(),
                                        toast_overlay.clone(),
                                        goa_client,
                                        goa_account,
                                        tasks_view.clone(),
                                        shared_owners,
                                    );
                                }
                                // ...and its Microsoft 365 contacts (item 5.2 /
                                // A-04), which had no session before consent.
                                if let (Some(goa_client), Some(contacts_account)) = (goa_client, contacts_account) {
                                    if !state.borrow().contact_cmd_tx.contains_key(&account_id) {
                                        let (cmd_tx, cmd_rx) = async_channel::unbounded();
                                        state.borrow_mut().contact_cmd_tx.insert(account_id.clone(), cmd_tx);
                                        sync_contacts_account(
                                            worker.clone(),
                                            state.clone(),
                                            toast_overlay.clone(),
                                            goa_client,
                                            contacts_account,
                                            cmd_rx,
                                            refresh_contacts_ui.clone(),
                                        );
                                        refresh_contacts_ui(None);
                                    }
                                }
                                let refresh = refresh_hook.borrow().clone();
                                refresh();
                            }
                            Err(e) => {
                                toast_overlay.add_toast(adw::Toast::new(&format!("Couldn't enable Outlook sync: {e}")));
                            }
                        }
                    });
                })
            };
            // The GOA enable/disable switches: persist the preference, then
            // connect the account's services back up (enable) or tear them
            // down and hide everything (disable).
            let toggle_goa: crate::config_view::AccountToggle = {
                let state = state.clone();
                let calendar_state = calendar_state.clone();
                let worker = worker.clone();
                let window = window.clone();
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_icon = mark_read_icon.clone();
                let pin_icon = pin_icon.clone();
                let root_stack = root_stack.clone();
                let current_mail_page = current_mail_page.clone();
                let mail_view_button = mail_view_button.clone();
                let current_calendar_page = current_calendar_page.clone();
                let calendar_view_button = calendar_view_button.clone();
                let current_tasks_page = current_tasks_page.clone();
                let tasks_view_button = tasks_view_button.clone();
                let current_lookout_page = current_lookout_page.clone();
                let lookout_view_button = lookout_view_button.clone();
                let calendar_main = calendar_main.clone();
                let calendar_list_box = calendar_list_box.clone();
                let mini_calendar = mini_calendar.clone();
                let mail_overview_day = mail_overview_day.clone();
                let mail_overview_day_list = mail_overview_day_list.clone();
                let reminders_engine = reminders_engine.clone();
                let tasks_view = tasks_view.clone();
                let refresh_contacts_ui = refresh_contacts_ui.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |account_id: &str, enabled: bool| {
                    let id = AccountId(account_id.to_string());
                    state.borrow().set_account_enabled(&id, enabled);
                    if enabled {
                        let discovered = state.borrow().goa_accounts.get(&id).cloned();
                        if let Some(discovered) = discovered {
                            connect_goa_account(
                                worker.clone(),
                                state.clone(),
                                calendar_state.clone(),
                                &id,
                                &discovered,
                                root_stack.clone(),
                                current_mail_page.clone(),
                                mail_view_button.clone(),
                                current_calendar_page.clone(),
                                calendar_view_button.clone(),
                                current_tasks_page.clone(),
                                tasks_view_button.clone(),
                                current_lookout_page.clone(),
                                lookout_view_button.clone(),
                                folder_selection.clone(),
                                folder_scroller.clone(),
                                message_list.clone(),
                                message_list_stack.clone(),
                                message_header.clone(),
                                reading_stack.clone(),
                                toast_overlay.clone(),
                                window.clone(),
                                app.clone(),
                                list_header.clone(),
                                dashboard_refresh.clone(),
                                mark_read_icon.clone(),
                                pin_icon.clone(),
                                calendar_main.clone(),
                                calendar_list_box.clone(),
                                mini_calendar.clone(),
                                mail_overview_day.clone(),
                                mail_overview_day_list.clone(),
                                reminders_engine.clone(),
                                tasks_view.clone(),
                                refresh_contacts_ui.clone(),
                            );
                        }
                    } else {
                        let email = state.borrow().goa_accounts.get(&id).map(|a| a.email.clone());
                        teardown_goa_account(
                            &state,
                            &calendar_state,
                            &id,
                            email.as_deref(),
                            &folder_selection,
                            &folder_scroller,
                            &message_list,
                            &message_list_stack,
                            &list_header,
                            &calendar_main,
                            &calendar_list_box,
                            &reminders_engine,
                            &tasks_view,
                            &refresh_contacts_ui,
                            &dashboard_refresh,
                        );
                        // A disabled account's events leave the desktop
                        // mirror straight away (EDS_TODO.md E2.4).
                        crate::eds_mirror::request_sync(&calendar_state, &worker, None);
                    }
                    // Repaint the Config account list so the switch's new
                    // state and the service lists underneath stay current.
                    let refresh = refresh_hook.borrow().clone();
                    refresh();
                })
            };
            // Config's per-account "Use Graph mail (experimental)" switch
            // (item 5.7.6 / A-06). Enabling needs Graph consent first; either
            // direction then tears the mail session down, drops the cache (the
            // two transports' folder and Uid spaces can't be merged) and
            // reconnects the same account so `connect_account` picks the
            // transport anew.
            let set_graph_mail: crate::config_view::GraphMailToggle = {
                let state = state.clone();
                let worker = worker.clone();
                let toast_overlay = toast_overlay.clone();
                let window = window.clone();
                let app = app.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_icon = mark_read_icon.clone();
                let pin_icon = pin_icon.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |account_id: &str, enabled: bool| {
                    let id = AccountId(account_id.to_string());
                    if state.borrow().app_config.borrow().graph_mail_enabled_for(&id) == enabled {
                        return;
                    }
                    let state = state.clone();
                    let worker = worker.clone();
                    let toast_overlay = toast_overlay.clone();
                    let window = window.clone();
                    let app = app.clone();
                    let folder_selection = folder_selection.clone();
                    let folder_scroller = folder_scroller.clone();
                    let message_list = message_list.clone();
                    let message_list_stack = message_list_stack.clone();
                    let message_header = message_header.clone();
                    let reading_stack = reading_stack.clone();
                    let list_header = list_header.clone();
                    let dashboard_refresh = dashboard_refresh.clone();
                    let mark_read_icon = mark_read_icon.clone();
                    let pin_icon = pin_icon.clone();
                    let refresh_hook = refresh_hook.clone();
                    glib::spawn_future_local(async move {
                        if enabled {
                            // Graph mail needs the Outlook-sync consent plus
                            // Mail.Send for its outbox (item 5.7.6). A grant
                            // that already carries both - a re-enable, or an
                            // earlier schedule-send consent - skips the
                            // browser prompt.
                            let oauth = crate::microsoft_oauth::MicrosoftOAuth::new(id.clone());
                            let consent = if oauth.graph_mail_consented() {
                                Ok(())
                            } else {
                                oauth.ensure_graph_mail_consent().await
                            };
                            if let Err(e) = consent {
                                let title = glib::markup_escape_text(&format!("Couldn't enable Graph mail: {e}"));
                                toast_overlay.add_toast(adw::Toast::new(&title));
                                // Put the switch back: nothing was saved.
                                let refresh = refresh_hook.borrow().clone();
                                refresh();
                                return;
                            }
                        }
                        {
                            let app_config = state.borrow().app_config.clone();
                            if !app_config.borrow_mut().set_graph_mail_enabled(&id, enabled) {
                                return;
                            }
                            crate::app_config::save(&app_config.borrow());
                        }
                        teardown_mail_account(
                            &state,
                            &id,
                            &folder_selection,
                            &folder_scroller,
                            &message_list,
                            &message_list_stack,
                            &list_header,
                            &dashboard_refresh,
                        );
                        // A cache from the other transport holds folder ids and
                        // Uids this one can't resolve; start it clean.
                        if let Err(e) = lookout_mail::remove_account_cache(&id) {
                            tracing::warn!(account_id = %id, "couldn't clear the mail cache for the transport switch: {e}");
                        }
                        let discovered = state.borrow().goa_accounts.get(&id).and_then(|account| account.mail.clone());
                        let goa_client = state.borrow().goa_client.clone();
                        if let (Some(mail), Some(goa_client)) = (discovered, goa_client) {
                            connect_account(
                                worker.clone(),
                                state.clone(),
                                folder_selection.clone(),
                                folder_scroller.clone(),
                                message_list.clone(),
                                message_list_stack.clone(),
                                message_header.clone(),
                                reading_stack.clone(),
                                toast_overlay.clone(),
                                window.clone(),
                                app.clone(),
                                goa_client,
                                list_header.clone(),
                                mail,
                                dashboard_refresh.clone(),
                                mark_read_icon.clone(),
                                pin_icon.clone(),
                            );
                        }
                        toast_overlay.add_toast(adw::Toast::new(if enabled {
                            "Graph mail enabled - new mail arrives by polling"
                        } else {
                            "Switched back to IMAP/SMTP"
                        }));
                        let refresh = refresh_hook.borrow().clone();
                        refresh();
                    });
                })
            };
            crate::config_view::refresh(
                &config_view,
                &goa,
                goa_unavailable,
                &mail,
                &calendar,
                &webcal,
                &mail_cache_dir,
                &mail_cache_files,
                &calendar_cache_dir,
                &calendar_cache_files,
                &contacts_cache_dir,
                &contacts_cache_files,
                &signatures_list,
                &signature_defaults,
                &templates_list,
                &manage_identities,
                &set_default_signature,
                &edit_signature,
                &remove_signature,
                &edit_template,
                &remove_template,
                &edit_other,
                &remove_other,
                &enable_graph_pin,
                &set_graph_mail,
                &toggle_goa,
            );
        }
    });
    // From here on, the manage-identities dialog's `on_changed` re-runs this
    // closure (and refreshes any open composer's From dropdown via
    // `composer_identities_refresh`), keeping the Config view's rows current
    // after an edit. (The `Rc` cycle this creates lives for the window's
    // lifetime, like the other closure cycles this file already accepts.)
    *refresh_hook.borrow_mut() = refresh_config.clone();
    // Populate the placeholder rows now (both groups are empty at startup).
    refresh_config();

    // What every new manual account does once saved - from "Add IMAP/SMTP
    // account…" or the Evolution import: connect it, leave the first-run page
    // for the mail view, and repaint Config.
    let connect_new_other_account: Rc<dyn Fn(&str)> = {
        let state = state.clone();
        let worker = worker.clone();
        let keyring = keyring.clone();
        let window = window.clone();
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        let list_header = list_header.clone();
        let dashboard_refresh = dashboard_refresh.clone();
        let mark_read_icon = mark_read_icon.clone();
        let pin_icon = pin_icon.clone();
        let refresh_hook = refresh_hook.clone();
        let root_stack = root_stack.clone();
        let current_mail_page = current_mail_page.clone();
        let mail_view_button = mail_view_button.clone();
        let current_lookout_page = current_lookout_page.clone();
        let lookout_view_button = lookout_view_button.clone();
        Rc::new(move |account_id: &str| {
            let Some(account) = state.borrow().app_config.borrow().other_account(account_id) else {
                return;
            };
            connect_other_account(
                worker.clone(),
                state.clone(),
                folder_selection.clone(),
                folder_scroller.clone(),
                message_list.clone(),
                message_list_stack.clone(),
                message_header.clone(),
                reading_stack.clone(),
                toast_overlay.clone(),
                window.clone(),
                app.clone(),
                list_header.clone(),
                account,
                keyring.clone(),
                dashboard_refresh.clone(),
                mark_read_icon.clone(),
                pin_icon.clone(),
            );
            // The first account added from the "No Mail Accounts" page: show
            // the mail it's about to sync rather than the empty state, as
            // account discovery does.
            if current_mail_page.get() == "empty" {
                current_mail_page.set("mail");
                if mail_view_button.is_active() {
                    root_stack.set_visible_child_name("mail");
                }
            }
            if current_lookout_page.get() == "lookout-empty" {
                current_lookout_page.set("lookout");
                if lookout_view_button.is_active() {
                    root_stack.set_visible_child_name("lookout");
                }
            }
            let refresh = refresh_hook.borrow().clone();
            refresh();
        })
    };

    // Config → Accounts → "Add IMAP/SMTP account…": the app's own manual
    // account entry point, alongside the GOA-settings "Add account…" row.
    {
        let worker = worker.clone();
        let state = state.clone();
        let keyring = keyring.clone();
        let connect_new_other_account = connect_new_other_account.clone();
        config_view.add_imap_row.connect_activated(move |row| {
            let app_config = state.borrow().app_config.clone();
            crate::other_accounts::show_add_account_dialog(
                row.upcast_ref::<gtk::Widget>(),
                worker.clone(),
                app_config,
                keyring.clone(),
                None,
                None,
                connect_new_other_account.clone(),
            );
        });
    }

    // "Import from Evolution…" (EDS_TODO.md E3.4), in Config → Accounts and on
    // the "No Mail Accounts" page. A background read of Evolution's accounts
    // (which never starts EDS) shows both only when one isn't in Lookout yet;
    // `refresh_hook` re-checks against the cached list whenever accounts are
    // added, removed or discovered.
    {
        let evolution_drafts: Rc<RefCell<Vec<crate::evolution_import::OtherAccountDraft>>> = Rc::new(RefCell::new(Vec::new()));
        // Every address Lookout already has an account for, beyond the manual
        // accounts `is_already_present` checks itself.
        let known_addresses = {
            let state = state.clone();
            move || -> Vec<String> { state.borrow().goa_accounts.values().map(|a| a.email.clone()).filter(|e| !e.is_empty()).collect() }
        };
        let refresh_import_visibility: Rc<dyn Fn()> = {
            let evolution_drafts = evolution_drafts.clone();
            let state = state.clone();
            let known_addresses = known_addresses.clone();
            let row = config_view.import_evolution_row.clone();
            let button = import_evolution_button.clone();
            Rc::new(move || {
                let known = known_addresses();
                let known: Vec<&str> = known.iter().map(String::as_str).collect();
                let app_config = state.borrow().app_config.clone();
                let visible = crate::evolution_import::any_importable(&evolution_drafts.borrow(), &app_config.borrow().other_accounts, &known);
                row.set_visible(visible);
                button.set_visible(visible);
            })
        };
        let (drafts_tx, drafts_rx) = async_channel::bounded(1);
        worker.spawn(async move {
            let _ = drafts_tx.send(crate::evolution_import::evolution_account_drafts().await).await;
        });
        glib::spawn_future_local({
            let evolution_drafts = evolution_drafts.clone();
            let refresh_import_visibility = refresh_import_visibility.clone();
            async move {
                let Ok(drafts) = drafts_rx.recv().await else { return };
                *evolution_drafts.borrow_mut() = drafts;
                refresh_import_visibility();
            }
        });
        // Chain onto the Config repaint every account change already runs.
        let refresh_config_only = refresh_hook.borrow().clone();
        *refresh_hook.borrow_mut() = {
            let refresh_import_visibility = refresh_import_visibility.clone();
            Rc::new(move || {
                refresh_config_only();
                refresh_import_visibility();
            })
        };

        // Opening re-reads Evolution, so the list is current, then shows the
        // dialog (or hides the entry points if nothing is left to import).
        let open_import: Rc<dyn Fn(&gtk::Widget)> = {
            let worker = worker.clone();
            let state = state.clone();
            let keyring = keyring.clone();
            let connect_new_other_account = connect_new_other_account.clone();
            Rc::new(move |anchor: &gtk::Widget| {
                let (tx, rx) = async_channel::bounded(1);
                worker.spawn(async move {
                    let _ = tx.send(crate::evolution_import::evolution_account_drafts().await).await;
                });
                let anchor = anchor.clone();
                let worker = worker.clone();
                let state = state.clone();
                let keyring = keyring.clone();
                let connect_new_other_account = connect_new_other_account.clone();
                let evolution_drafts = evolution_drafts.clone();
                let refresh_import_visibility = refresh_import_visibility.clone();
                let known_addresses = known_addresses.clone();
                glib::spawn_future_local(async move {
                    let Ok(drafts) = rx.recv().await else { return };
                    *evolution_drafts.borrow_mut() = drafts.clone();
                    refresh_import_visibility();
                    if drafts.is_empty() {
                        return;
                    }
                    let app_config = state.borrow().app_config.clone();
                    crate::evolution_import::show_import_dialog(&anchor, worker, app_config, keyring, drafts, known_addresses(), connect_new_other_account);
                });
            })
        };
        {
            let open_import = open_import.clone();
            config_view.import_evolution_row.connect_activated(move |row| open_import(row.upcast_ref()));
        }
        import_evolution_button.connect_clicked(move |button| open_import(button.upcast_ref()));
    }

    // Config → Accounts → "Add shared mailbox…" (item 5.6 / M-42): manage the
    // shared mailboxes Lookout connects against a Microsoft 365 account.
    {
        let add_shared_mailbox_row = config_view.add_shared_mailbox_row.clone();
        let worker = worker.clone();
        let state = state.clone();
        let window = window.clone();
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        let list_header = list_header.clone();
        let dashboard_refresh = dashboard_refresh.clone();
        let mark_read_icon = mark_read_icon.clone();
        let pin_icon = pin_icon.clone();
        let refresh_hook = refresh_hook.clone();
        add_shared_mailbox_row.connect_activated(move |row| {
            let app_config = state.borrow().app_config.clone();
            let parents: Vec<crate::shared_mailbox::SharedMailboxParent> = state
                .borrow()
                .goa_accounts
                .values()
                .filter_map(|discovered| {
                    discovered
                        .mail
                        .as_ref()
                        .filter(|mail| mail.is_microsoft_365())
                        .map(|mail| crate::shared_mailbox::SharedMailboxParent {
                            account_id: mail.account_id.clone(),
                            label: format!("{} <{}>", discovered.display_name, discovered.email),
                        })
                })
                .collect();
            let on_changed = {
                let state = state.clone();
                let worker = worker.clone();
                let window = window.clone();
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_icon = mark_read_icon.clone();
                let pin_icon = pin_icon.clone();
                let refresh_hook = refresh_hook.borrow().clone();
                Rc::new(move || {
                    let Some(goa_client) = state.borrow().goa_client.clone() else { return };
                    let parents: Vec<(lookout_core::AccountId, lookout_goa::GoaMailAccount)> = state
                        .borrow()
                        .goa_accounts
                        .values()
                        .filter_map(|discovered| discovered.mail.clone().filter(lookout_goa::GoaMailAccount::is_microsoft_365))
                        .map(|mail| (mail.account_id.clone(), mail))
                        .collect();
                    for (parent_id, parent) in parents {
                        crate::window::connect::connect_shared_mailboxes(
                            worker.clone(),
                            state.clone(),
                            folder_selection.clone(),
                            folder_scroller.clone(),
                            message_list.clone(),
                            message_list_stack.clone(),
                            message_header.clone(),
                            reading_stack.clone(),
                            toast_overlay.clone(),
                            window.clone(),
                            app.clone(),
                            goa_client.clone(),
                            list_header.clone(),
                            dashboard_refresh.clone(),
                            mark_read_icon.clone(),
                            pin_icon.clone(),
                            &parent_id,
                            &parent,
                        );
                    }
                    refresh_hook();
                })
            };
            crate::shared_mailbox::show_manage_shared_mailboxes(row.upcast_ref::<gtk::Widget>(), app_config, parents, on_changed);
        });
    }

    // Config → Accounts → Signatures → "New Signature…": the rich-text
    // signature editor. Saving writes through to `app_config` and re-runs
    // this closure (via `refresh_hook`) so the list below the button
    // redraws with the new entry.
    {
        let new_signature_row = config_view.new_signature_row.clone();
        let state = state.clone();
        let refresh_hook = refresh_hook.clone();
        new_signature_row.connect_activated(move |row| {
            let app_config = state.borrow().app_config.clone();
            let on_changed = {
                let refresh_hook = refresh_hook.borrow().clone();
                Rc::new(move || refresh_hook())
            };
            crate::signatures::show_editor_dialog(row.upcast_ref::<gtk::Widget>(), app_config, None, on_changed);
        });
    }

    // Config → Mail → My Templates → "New template…" (item 2.10 / C-17): the
    // rich-text template editor. Saving writes through to `app_config` and
    // re-runs this closure (via `refresh_hook`) so the list below the button
    // redraws with the new entry.
    {
        let new_template_row = config_view.new_template_row.clone();
        let state = state.clone();
        let refresh_hook = refresh_hook.clone();
        new_template_row.connect_activated(move |row| {
            let app_config = state.borrow().app_config.clone();
            let on_changed = {
                let refresh_hook = refresh_hook.borrow().clone();
                Rc::new(move || refresh_hook())
            };
            crate::templates::show_editor_dialog(row.upcast_ref::<gtk::Widget>(), app_config, None, on_changed);
        });
    }

    // --- Settings window: the header bar's Settings button opens the Config
    // screen (the same sidebar/stack layout that used to live in the nav
    // rail) as a modal dialog over the main window. Single instance: while
    // one is open, re-clicking the button (or the Ctrl+6 shortcut below)
    // just re-presents it. The window is destroyed on close and rebuilt on
    // the next open around the same shared `config_view.root` widget, so no
    // per-session state is lost. The root is detached *explicitly* rather
    // than relying on the destroy cascade to unparent it: closure reference
    // cycles into the window's content chain (the close button's handler
    // holds the dialog, the dialog holds the chain) can keep the chain
    // alive after the window is destroyed, leaving the root parented to a
    // dead window - and the next open then fails to attach it, showing an
    // empty modal. `refresh_config` runs on every open so the account lists
    // are current, and the discovery passes keep refreshing it while the
    // dialog is up. ---
    {
        let window = window.clone();
        let config_view = config_view.clone();
        let refresh_config = refresh_config.clone();
        let config_window: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
        let open_settings = {
            let window = window.clone();
            let config_view = config_view.clone();
            let refresh_config = refresh_config.clone();
            let config_window = config_window.clone();
            move || {
                if let Some(existing) = config_window.borrow().as_ref() {
                    existing.present();
                    return;
                }
                // Defensive: if the previous window's chain somehow still
                // parents the root, drop that link first so the attach below
                // can't fail. Safe no-op when the root is already detached.
                config_view.root.unparent();
                let dialog = adw::Window::builder()
                    .transient_for(&window)
                    .modal(true)
                    .title("Settings")
                    .default_width(1000)
                    .default_height(720)
                    .build();
                let header = adw::HeaderBar::new();
                let settings_title = adw::WindowTitle::new("Settings", "");
                crate::privacy::mark_ui(&settings_title);
                header.set_title_widget(Some(&settings_title));
                let close_button = gtk::Button::from_icon_name("window-close-symbolic");
                close_button.set_css_classes(&["flat"]);
                close_button.set_tooltip_text(Some("Close"));
                header.pack_end(&close_button);
                let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
                content.append(&header);
                // Same card treatment the Config screen had in the main
                // window, so the dialog shows the identical design.
                let config_card = card_section(&config_view.root);
                config_card.add_css_class("folder-pane");
                config_card.set_vexpand(true);
                content.append(&config_card);
                dialog.set_content(Some(&content));
                {
                    let dialog = dialog.clone();
                    close_button.connect_clicked(move |_| dialog.close());
                }
                {
                    let config_window = config_window.clone();
                    let config_view = config_view.clone();
                    dialog.connect_close_request(move |_| {
                        // Unparent the shared root now, while the window is
                        // still alive: the destroy cascade may never reach it
                        // if a reference cycle keeps this window's content
                        // chain alive, and the next open then can't re-attach
                        // it (the modal would open empty).
                        config_view.root.unparent();
                        *config_window.borrow_mut() = None;
                        glib::Propagation::Proceed
                    });
                }
                refresh_config();
                *config_window.borrow_mut() = Some(dialog.clone());
                dialog.present();
            }
        };
        settings_button.connect_clicked({
            let open_settings = open_settings.clone();
            move |_| open_settings()
        });
        // Ctrl+6 (shortcuts.rs ACTION_CONFIG) opens the same dialog. The
        // rail-toggle loop above can't carry it - `refresh_config` only
        // exists from here on - so it's dispatched here instead. The same
        // `open_settings` rides as the composer's "Manage Signatures…"
        // hook, which additionally jumps the settings screen to the
        // Accounts section (where the global signature list lives).
        let open_settings = Rc::new(open_settings);
        dispatch.borrow_mut().insert(crate::shortcuts::ACTION_CONFIG, open_settings.clone());
        state.borrow_mut().manage_signatures = Some(Rc::new({
            let open_settings = open_settings.clone();
            let config_view = config_view.clone();
            move || {
                open_settings();
                crate::config_view::show_section(&config_view, "accounts");
            }
        }));
        // The ribbon's Send / Receive ▸ Account Settings opens the same
        // dialog on its Accounts section.
        account_settings_button.connect_clicked({
            let open_settings = open_settings.clone();
            let config_view = config_view.clone();
            move |_| {
                open_settings();
                crate::config_view::show_section(&config_view, "accounts");
            }
        });
        // The composer's "Manage Templates…" hook jumps to the Mail section,
        // where the My Templates list lives (item 2.10 / C-17).
        state.borrow_mut().manage_templates = Some(Rc::new({
            let open_settings = open_settings.clone();
            let config_view = config_view.clone();
            move || {
                open_settings();
                crate::config_view::show_section(&config_view, "mail");
            }
        }));
    }

    // --- "Clear all caches" (Config → Advanced): deletes the on-disk mail,
    // calendar and contacts caches, drops the in-memory calendar occurrences,
    // and asks every connected account to resync so the caches rebuild from
    // the servers right away rather than on next launch ---
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let toast_overlay = toast_overlay.clone();
        let worker = worker.clone();
        config_view.clear_cache_row.connect_activated(move |_| {
            match (lookout_mail::clear_all_caches(), lookout_dav::clear_all_caches()) {
                (Ok(()), Ok(())) => toast_overlay.add_toast(adw::Toast::new("Cleared email, calendar and contacts caches")),
                (Err(e), _) => {
                    let title = glib::markup_escape_text(&format!("Couldn't clear caches: {e}"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
                (_, Err(e)) => {
                    let title = glib::markup_escape_text(&format!("Couldn't clear caches: {e}"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
            }
            // The mirror is derived from the calendar caches, so clear it too
            // and let the request below (and the month resyncs after it)
            // refill it from scratch (EDS_TODO.md E2.4).
            crate::eds_mirror::clear_mirror_file();
            crate::eds_mirror::request_sync(&calendar_state, &worker, None);
            let month = calendar_state.borrow().displayed_month;
            for handle in calendar_state.borrow_mut().accounts.values_mut() {
                handle.last_occurrences.clear();
                handle.last_synced_month = None;
                handle.occurrences_by_month.clear();
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
            }
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
            refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
            for handle in state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::Refresh);
            }
            let account_ids: Vec<AccountId> = state.borrow().accounts.keys().cloned().collect();
            for account_id in &account_ids {
                sync_mail_properties_from_graph(&state, account_id);
                sync_and_push_rules_from_graph(&state, account_id);
                sync_automatic_replies_from_graph(&state, account_id);
            }
            push_sieve_script(&state);
            for handle in state.borrow().accounts.values().filter(|handle| handle.sieve_advertised) {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::FetchSieveCapabilities);
            }
            refresh_automatic_replies_ui(&state);
            for cmd_tx in state.borrow().contact_cmd_tx.values() {
                let _ = cmd_tx.send_blocking(ContactCommand::Refresh);
            }
        });
    }

    // --- Config → Google Tasks → OAuth client id: shows the configured id
    // (or its absence) and opens a small entry dialog to set/clear it - the
    // GUI-friendly way to provide the client id Google requires, instead of
    // an environment variable.
    {
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let google_tasks_client_row = config_view.google_tasks_client_row.clone();
        crate::config_view::refresh_google_tasks_client_row(&google_tasks_client_row);
        let row_for_dialog = google_tasks_client_row.clone();
        google_tasks_client_row.connect_activated(move |_| {
            show_google_tasks_client_id_dialog(&window, &toast_overlay, &row_for_dialog);
        });
    }

    // --- Config → Assistant: the OpenAI-compatible endpoint the assistant
    // talks to. The URL lives in GSettings (`assistant-api-url`); the token
    // lives in the GNOME keyring (`assistant.rs`), never in dconf or
    // settings.json. Edits persist live: the URL writes straight through on
    // every change, while the token's keyring write is debounced - each
    // write opens a fresh Secret Service connection, so per-keystroke
    // writes would be too noisy. The "Test connection" button runs the
    // `GET {url}/models` probe on the worker and reports the outcome in the
    // row's subtitle.
    {
        let settings = state.borrow().settings.clone();
        let assistant_url_entry = config_view.assistant_url_entry.clone();
        let assistant_token_row = config_view.assistant_token_row.clone();
        let assistant_test_row = config_view.assistant_test_row.clone();
        let assistant_test_button = config_view.assistant_test_button.clone();
        let assistant_agent_row = config_view.assistant_agent_row.clone();
        let assistant_agent_model = config_view.assistant_agent_model.clone();
        let assistant_agent_refresh_button = config_view.assistant_agent_refresh_button.clone();

        // Seed the URL before wiring the `changed` handler so the seed's
        // own signal can't fire a redundant write. The token seed is async
        // (keyring), so its `set_text` runs behind a guard flag that
        // suppresses the debounced write-back (and skips seeding over
        // anything the user has already typed).
        assistant_url_entry.set_text(&settings.get_string(crate::settings::ASSISTANT_API_URL));
        assistant_test_button.set_sensitive(!assistant_url_entry.text().trim().is_empty());
        let pending_token_write: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let seeding_token = Rc::new(Cell::new(true));
        {
            let worker = worker.clone();
            let token_row = assistant_token_row.clone();
            let seeding_token = seeding_token.clone();
            let (result_tx, result_rx) = async_channel::bounded(1);
            worker.spawn(async move {
                let _ = result_tx.send(crate::assistant::load_token().await).await;
            });
            glib::spawn_future_local(async move {
                let Ok(result) = result_rx.recv().await else { return };
                match result {
                    Ok(Some(token)) if token_row.text().is_empty() => {
                        token_row.set_text(&token);
                    }
                    Ok(Some(_)) | Ok(None) => {}
                    Err(e) => tracing::warn!("could not load the Assistant API token from the keyring: {e}"),
                }
                seeding_token.set(false);
            });
        }
        // The URL's `changed` handler writes through and arms/disarms the
        // Test and Refresh buttons with the URL's presence. Changing the
        // URL also clears the agent list - another server's agents are
        // meaningless.
        {
            let settings = settings.clone();
            let assistant_test_button = assistant_test_button.clone();
            let assistant_agent_row = assistant_agent_row.clone();
            let assistant_agent_model = assistant_agent_model.clone();
            let assistant_agent_refresh_button = assistant_agent_refresh_button.clone();
            assistant_url_entry.connect_changed(move |entry| {
                let url = entry.text().trim().to_string();
                settings.set_string(crate::settings::ASSISTANT_API_URL, &url);
                assistant_test_button.set_sensitive(!url.is_empty());
                assistant_agent_refresh_button.set_sensitive(!url.is_empty());
                assistant_agent_model.splice(0, assistant_agent_model.n_items(), &[]);
                assistant_agent_row.set_subtitle("Refresh to list the API's available agents");
                assistant_agent_row.set_sensitive(false);
            });
        }
        // The "Agent" dropdown: the refresh closure loads the API's model
        // list (the same `/models` probe the Test button runs, with the
        // keyring token) into the dropdown, re-asserting the stored
        // selection, and reports the outcome in the row's subtitle. Shared
        // by the Refresh button and by a successful Test connection.
        let refresh_agents: Rc<dyn Fn()> = {
            let worker = worker.clone();
            let settings = settings.clone();
            let assistant_url_entry = assistant_url_entry.clone();
            let assistant_agent_row = assistant_agent_row.clone();
            let assistant_agent_model = assistant_agent_model.clone();
            let assistant_agent_refresh_button = assistant_agent_refresh_button.clone();
            Rc::new(move || {
                let url = assistant_url_entry.text().trim().to_string();
                if url.is_empty() {
                    return;
                }
                assistant_agent_refresh_button.set_sensitive(false);
                assistant_agent_row.set_subtitle("Loading agents…");
                let (result_tx, result_rx) = async_channel::bounded(1);
                worker.spawn(async move {
                    let result = async {
                        let token = crate::assistant::load_token().await?.unwrap_or_default();
                        crate::assistant::list_models(&url, &token).await
                    }
                    .await;
                    let _ = result_tx.send(result).await;
                });
                let assistant_agent_row = assistant_agent_row.clone();
                let assistant_agent_model = assistant_agent_model.clone();
                let assistant_agent_refresh_button = assistant_agent_refresh_button.clone();
                let settings = settings.clone();
                glib::spawn_future_local(async move {
                    let Ok(result) = result_rx.recv().await else { return };
                    match result {
                        Ok(models) => {
                            let additions: Vec<&str> = models.iter().map(String::as_str).collect();
                            assistant_agent_model.splice(0, assistant_agent_model.n_items(), &additions);
                            let subtitle = if models.is_empty() {
                                "No agents available".to_string()
                            } else if models.len() == 1 {
                                "1 agent available".to_string()
                            } else {
                                format!("{} agents available", models.len())
                            };
                            assistant_agent_row.set_subtitle(&subtitle);
                            assistant_agent_row.set_sensitive(!models.is_empty());
                            let stored = settings.get_string(crate::settings::ASSISTANT_AGENT);
                            if let Some(index) = models.iter().position(|m| *m == stored) {
                                assistant_agent_row.set_selected(index as u32);
                            }
                        }
                        Err(e) => assistant_agent_row.set_subtitle(&format!("Failed: {e}")),
                    }
                    assistant_agent_refresh_button.set_sensitive(true);
                });
            })
        };
        // The token's `changed` handler debounces the keyring write: a
        // pending write is cancelled and rescheduled, so the keyring is
        // only touched once typing settles (or the field is cleared, which
        // deletes the stored token).
        {
            let worker = worker.clone();
            let pending_token_write = pending_token_write.clone();
            let seeding_token = seeding_token.clone();
            assistant_token_row.connect_changed(move |row| {
                if seeding_token.get() {
                    return;
                }
                if let Some(source) = pending_token_write.borrow_mut().take() {
                    source.remove();
                }
                let worker = worker.clone();
                let token = row.text().to_string();
                let pending_token_write_done = pending_token_write.clone();
                *pending_token_write.borrow_mut() = Some(glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                    // The source has already fired, so GLib has destroyed it;
                    // clear the handle now or a later keystroke's `.take()`
                    // will try to remove a source that no longer exists and
                    // panic in the (non-unwindable) GTK signal trampoline.
                    pending_token_write_done.borrow_mut().take();
                    worker.spawn(async move {
                        let result = if token.is_empty() {
                            crate::assistant::delete_token().await
                        } else {
                            crate::assistant::store_token(&token).await
                        };
                        if let Err(e) = result {
                            tracing::warn!("could not save the Assistant API token to the keyring: {e}");
                        }
                    });
                }));
            });
        }
        // The Test button: run the probe on the worker (keyring token load
        // + the HTTP GET), then report the outcome in the row's subtitle.
        // A successful connection also refreshes the agent list - the
        // connection is known good then, so the dropdown fills itself.
        {
            let worker = worker.clone();
            let assistant_url_entry = assistant_url_entry.clone();
            let assistant_test_row = assistant_test_row.clone();
            let assistant_test_button = assistant_test_button.clone();
            let refresh_agents = refresh_agents.clone();
            assistant_test_button.connect_clicked(move |button| {
                let url = assistant_url_entry.text().trim().to_string();
                if url.is_empty() {
                    return;
                }
                button.set_sensitive(false);
                assistant_test_row.set_subtitle("Testing…");
                let (result_tx, result_rx) = async_channel::bounded(1);
                worker.spawn(async move {
                    let result = async {
                        let token = crate::assistant::load_token().await?.unwrap_or_default();
                        crate::assistant::test_connection(&url, &token).await
                    }
                    .await;
                    let _ = result_tx.send(result).await;
                });
                let assistant_test_row = assistant_test_row.clone();
                let button = button.clone();
                let refresh_agents = refresh_agents.clone();
                glib::spawn_future_local(async move {
                    let Ok(result) = result_rx.recv().await else { return };
                    match result {
                        Ok(()) => {
                            assistant_test_row.set_subtitle("Connection OK");
                            refresh_agents();
                        }
                        Err(e) => assistant_test_row.set_subtitle(&format!("Failed: {e}")),
                    }
                    button.set_sensitive(true);
                });
            });
        }
        // The Refresh button triggers the agent-list load; the selection
        // persists live to the `assistant-agent` GSettings key.
        {
            let refresh_agents = refresh_agents.clone();
            assistant_agent_refresh_button.connect_clicked(move |_| refresh_agents());
        }
        {
            let settings = settings.clone();
            let assistant_agent_model = assistant_agent_model.clone();
            assistant_agent_row.connect_selected_notify(move |row| {
                let selected = row.selected();
                if selected != gtk::INVALID_LIST_POSITION {
                    if let Some(id) = assistant_agent_model.string(selected) {
                        settings.set_string(crate::settings::ASSISTANT_AGENT, &id);
                    }
                }
            });
        }
    }

    // --- Lookout tab → AI Chat: the card's Go button (and the prompt
    // field's Enter key) asks the configured assistant - the Settings →
    // Assistant URL, agent, and keyring token - and shows the reply in the
    // card's HTML output view (split into section cards by
    // `chat_reply_cards_html`, loaded through the reading pane's CSP
    // wrapper so the agent can use formatting and graphics but never
    // scripts). The prompt defaults to
    // "Summarize my inbox" when the field is left empty. The assistant
    // answers with function calling over the same local data the dashboard
    // shows: the mail caches, the contact snapshots, and the merged task
    // set (see `assistant_tools`), captured fresh at ask time so the reply
    // reflects the latest syncs.
    {
        let settings = state.borrow().settings.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let worker = worker.clone();
        let chat_entry = lookout_view.chat_entry.clone();
        let chat_button = lookout_view.chat_button.clone();
        let chat_output = lookout_view.chat_output.clone();
        let chat_button_for_click = chat_button.clone();
        let chat_entry_for_activate = chat_entry.clone();
        let ask_assistant: Rc<dyn Fn()> = Rc::new(move || {
            let prompt = chat_entry.text().trim().to_string();
            let prompt = if prompt.is_empty() { "Summarize my inbox".to_string() } else { prompt };
            chat_button.set_sensitive(false);
            chat_output.load_html(&crate::lookout_view::chat_loading_document(), None);
            let context = {
                // The upcoming-occurrences union the dashboard's own
                // "Upcoming events" card reads from (`window.rs`'s
                // Lookout-tab repaint) - gathered here in its own scope and
                // dropped before `merged_tasks` re-borrows `calendar_state`.
                let st = calendar_state.borrow();
                let occurrences: Vec<EventOccurrence> = st
                    .accounts
                    .values()
                    .flat_map(|h| h.occurrences_by_month.values().flatten().cloned())
                    .chain(st.webcal_handles.values().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
                    .chain(st.birthdays.as_ref().into_iter().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
                    .collect();
                let checked_calendars = st.checked_calendar_ids.clone();
                drop(st);
                crate::assistant_tools::ToolContext {
                    caches: state.borrow().accounts.values().filter_map(|h| h.address_cache.clone()).collect(),
                    contacts: state.borrow().contacts_by_account.iter().map(|(id, snapshot)| (id.clone(), snapshot.clone())).collect(),
                    tasks: merged_tasks(&calendar_state),
                    occurrences,
                    checked_calendars,
                }
            };
            let url = settings.get_string(crate::settings::ASSISTANT_API_URL);
            let agent = settings.get_string(crate::settings::ASSISTANT_AGENT);
            let (result_tx, result_rx) = async_channel::bounded(1);
            worker.spawn(async move {
                let result = async {
                    let token = crate::assistant::load_token().await?.unwrap_or_default();
                    crate::assistant_tools::chat_with_tools(&url, &token, &agent, &prompt, crate::assistant_tools::SYSTEM_PROMPT, &context).await
                }
                .await;
                let _ = result_tx.send(result).await;
            });
            let chat_output = chat_output.clone();
            let chat_button = chat_button.clone();
            glib::spawn_future_local(async move {
                let Ok(result) = result_rx.recv().await else { return };
                let document = match result {
                    Ok(reply) => crate::lookout_view::chat_document(&crate::lookout_view::chat_reply_cards_html(&reply)),
                    Err(e) => crate::lookout_view::chat_error_document(&e),
                };
                chat_output.load_html(&document, None);
                chat_button.set_sensitive(true);
            });
        });
        {
            let ask_assistant = ask_assistant.clone();
            chat_button_for_click.connect_clicked(move |_| ask_assistant());
        }
        {
            let ask_assistant = ask_assistant.clone();
            chat_entry_for_activate.connect_activate(move |_| ask_assistant());
        }
    }

    // --- AI Chat link clicks. `chat_output` has no `decide-policy` handling
    // of its own (unlike the reading pane's `web_view`, wired near the top
    // of this function): a `lookout-action:<action>?data=...` link (see
    // `chat_links`) dispatches straight to the matching `GAction` -
    // `open-event` (unchanged, reused from `reminders.rs`) or `open-message`
    // (registered above) - and anything else falls back to
    // `open_uri_in_default_browser`, the same treatment the reading pane
    // gives an ordinary link. The WebView itself never navigates either way.
    //
    // Critical: every `chat_output.load_html(...)` call (the "Asking the
    // assistant…" placeholder, then the real reply) is *itself* a
    // NavigationAction decision, not just a real click - exactly the
    // distinction the reading pane's handler documents and relies on
    // (`is_link_click`/`is_user_gesture`, above). Vetoing unconditionally
    // here silently drops every `load_html`, so the pane never renders
    // anything at all: only a genuine link click (`LinkClicked` or a user
    // gesture) is intercepted; anything else falls through to `false` so
    // WebKit's normal load proceeds.
    {
        let app_for_chat_links = app.clone();
        lookout_view.chat_output.connect_decide_policy(move |_view, decision, decision_type| {
            if decision_type != webkit::PolicyDecisionType::NavigationAction {
                return false;
            }
            let Some(navigation) = decision.downcast_ref::<webkit::NavigationPolicyDecision>().and_then(|d| d.navigation_action()) else {
                return false;
            };
            let is_link_click = navigation.navigation_type() == webkit::NavigationType::LinkClicked;
            let is_user_gesture = navigation.is_user_gesture();
            tracing::debug!(
                "AI Chat chat_output navigation: type={:?} is_link_click={is_link_click} is_user_gesture={is_user_gesture} uri={:?}",
                navigation.navigation_type(),
                navigation.request().and_then(|r| r.uri())
            );
            if !(is_link_click || is_user_gesture) {
                // The programmatic `load_html()` call itself - let it load.
                return false;
            }
            let Some(uri) = navigation.request().and_then(|r| r.uri()) else {
                tracing::debug!("AI Chat link click had no request URI");
                return false;
            };
            if let Some((action, payload)) = crate::chat_links::parse(&uri) {
                tracing::debug!("AI Chat link click: dispatching app.{action} (payload {payload:?})");
                app_for_chat_links.activate_action(&action, Some(&glib::Variant::from(payload)));
            } else {
                tracing::debug!("AI Chat link click: not a lookout-action link, opening externally: {uri}");
                open_uri_in_default_browser(&uri);
            }
            decision.ignore();
            true
        });
    }

    // --- Compose button -> new-message composer in the reading pane,
    // "From" = the account owning the selected message (falling back to the
    // currently-open mailbox's account, then any connected account). The
    // account choice is shared with the tray icon's "Compose…" menu item
    // through `compose_new_message`.
    {
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        let message_list = message_list.clone();
        compose_button.connect_clicked(move |_| {
            compose_new_message(&state, &worker, &reading_stack, &message_list);
        });
    }

    // --- Reply/Reply-All/Forward -> opens the composer in the reading pane
    // pre-filled from whatever message is currently selected and has a body
    // loaded. Silent no-op if nothing's selected or the body hasn't arrived
    // yet (same convention as the Delete/Archive/Report/Snooze buttons below).
    // Wired to three copies of each button - the top command toolbar, the
    // reading-pane header, and the bottom action bar below the message body -
    // by design, see the plan this shipped under.
    for (button, mode, title) in [
        (&reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&message_header.reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&message_header.bottom_reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
        (&message_header.reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
    ] {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        button.connect_clicked(move |_| {
            if let Some((summary, body, from_email, cmd_tx)) = selected_message_reply_context(&message_list, &state) {
                let prefill = crate::compose::build_reply_prefill(&summary, &body, &from_email, mode);
                let rich_text_default = state.borrow().rich_text_default;
                show_composer_in_reading_pane(
                    &state,
                    &worker,
                    &reading_stack,
                    title,
                    cmd_tx,
                    prefill,
                    rich_text_default,
                    mailbox_account_id(&summary.mailbox),
                    Some(crate::config_view::SignatureDefault::Reply),
                );
            }
        });
    }
    // The ribbon's Forward split button: the main action re-attaches the
    // original's parts (its dropdown's "Forward as attachment" is wired with
    // the button above).
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        forward_button.connect_clicked(move |_| {
            if let Some((summary, body, _from_email, _cmd_tx)) = selected_message_reply_context(&message_list, &state) {
                forward_message(&state, &worker, &reading_stack, &summary, &body, false);
            }
        });
    }
    // The reading-pane header and bottom-bar Forward buttons.
    for button in [&message_header.forward_button, &message_header.bottom_forward_button] {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        button.connect_clicked(move |_| {
            if let Some((summary, body, _from_email, _cmd_tx)) = selected_message_reply_context(&message_list, &state) {
                forward_message(&state, &worker, &reading_stack, &summary, &body, false);
            }
        });
    }

    // --- "View contact" (reading-pane header): looks the selected message's
    // sender up across every connected contacts account. Found - the contact
    // editor opens for that card (edit mode, routed to its own account).
    // Not found - the create editor opens prefilled with the sender, its
    // address-book picker listing every account's books with the account the
    // email lives in first. Silent no-op when nothing's selected or the
    // message has no sender.
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let message_list = message_list.clone();
        message_header.contact_button.connect_clicked(move |_| {
            let summary = match message_list.selected_summary() {
                Some(summary) => summary,
                None => return,
            };
            let Some(sender) = summary.from.first() else { return };
            let account_id = mailbox_account_id(&summary.mailbox);
            match find_contact_by_address(&state, &sender.address, account_id.as_ref()) {
                Some((_account_id, entry)) => show_contact_editor_for(&window, &state, &toast_overlay, &entry),
                None => show_create_contact_for(&window, &state, &toast_overlay, sender, account_id),
            }
        });
    }

    // --- "Switch message theme" toggle: arms/removes the background-stripping
    // user stylesheet and re-renders the open message so the override applies
    // to it, not just the next selection. A per-email override: every
    // navigation reset sets `message_theme_override` to the Config →
    // Appearance "Dark message theme" default (see `set_message_theme_armed`
    // for the physical side), and `render_body` syncs this button from it,
    // so the next message always opens in the configured default rather than
    // inheriting the previous message's manual override. The idempotence
    // guard up front makes the handler a no-op when state and button already
    // agree - which is what happens when `render_body`'s sync flips the
    // button after a navigation, so that sync-driven `toggled` can't trigger
    // a redundant re-render (or a loop). Re-rendering is skipped while a
    // composer is up, the same as the "Load images" toggle - see
    // `rerender_current_message`. Stripping the email's backgrounds alone is
    // not enough: the WebView's own page canvas paints white behind the (now
    // transparent) document, so the toggle also flips the canvas colour -
    // transparent while armed (letting the reading card's theme background
    // show through), white otherwise, matching WebKit's default so
    // un-backgrounded emails look exactly as they did before the toggle
    // existed.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        let message_list = message_list.clone();
        let web_view = web_view.clone();
        let user_content_manager = user_content_manager.clone();
        let theme_override_sheet = theme_override_sheet.clone();
        message_header.theme_button.clone().connect_toggled(move |button| {
            // The icon flips with the state - a moon while the override is
            // off, a sun while it's on (the sun also trails the moon's
            // candidate list as the no-moon fallback, see `message_header`).
            // The tooltip names what clicking the button will do next, so it
            // flips too - and both are set before the guard below so
            // `render_body`'s sync-driven `set_active` (which also lands
            // here) updates them.
            button.set_icon_name(if button.is_active() {
                crate::window::themed_icon_name(&["weather-clear-symbolic", "display-brightness-symbolic"])
            } else {
                crate::window::themed_icon_name(&["weather-clear-night-symbolic", "night-light-symbolic", "weather-clear-symbolic"])
            });
            button.set_tooltip_text(Some(if button.is_active() { "View in Light Mode" } else { "View in Dark Mode" }));
            if button.is_active() == state.borrow().message_theme_override {
                return;
            }
            set_message_theme_armed(button.is_active(), &web_view, &user_content_manager, &theme_override_sheet);
            state.borrow_mut().message_theme_override = button.is_active();
            rerender_current_message(&state, &reading_stack, &message_header, &message_list);
        });
    }

    // --- Debug: open a raw .eml fixture straight into the reading pane ---
    #[cfg(debug_assertions)]
    {
        // Shared by the debug opener button and the `LOOKOUT_DEBUG_EML` env
        // hook below: parse a fixture file and render it into the reading
        // pane. There's no IMAP session behind it, so the reading pane's cid:
        // scheme handler can't fetch inline images - rewrite them to `data:`
        // URIs straight from the raw message so fixtures render in full.
        fn render_fixture(path: &std::path::Path, state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, message_header: &crate::message_header::MessageHeader) {
            let Ok(raw) = std::fs::read(path) else { return };
            if let Some(body) = lookout_mail::body::parse_body(lookout_core::Uid(0), &raw) {
                let body = if let Some(html) = body.html_body.as_deref() {
                    lookout_core::EmailBody {
                        html_body: Some(lookout_mail::body::rewrite_cid_refs_to_data_uris(html, &raw)),
                        ..body
                    }
                } else {
                    body
                };
                render_body(state, reading_stack, message_header, MailboxId("debug:eml".into()), body.uid, Rc::new(body));
            }
        }

        let window = window.clone();
        let state = state.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        // The env hook's own captures - it fires after the button's closure
        // has taken the originals above.
        let env_state = state.clone();
        let env_message_header = message_header.clone();
        let env_reading_stack = reading_stack.clone();
        open_eml_button.connect_clicked(move |_| {
            let window = window.clone();
            let state = state.clone();
            let message_header = message_header.clone();
            let reading_stack = reading_stack.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("eml");
                filter.set_name(Some("Email messages (*.eml)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);

                let dialog = gtk::FileDialog::builder().title("Open .eml (debug)").filters(&filters).build();

                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                render_fixture(&path, &state, &reading_stack, &message_header);
            });
        });

        // `LOOKOUT_DEBUG_EML`: render the named fixture at startup instead of
        // clicking through the dialog - the headless path for reproducing a
        // fixture's breakage.
        if let Ok(path) = std::env::var("LOOKOUT_DEBUG_EML") {
            glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                render_fixture(std::path::Path::new(&path), &env_state, &env_reading_stack, &env_message_header);
            });
        }
    }

    // --- Network connectivity -> nudge every backed-off account to retry now ---
    {
        let state = state.clone();
        let monitor = gio::NetworkMonitor::default();
        monitor.connect_network_changed(move |_monitor, available| {
            if !available {
                return;
            }
            for handle in state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::Reconnect);
            }
        });
    }

    // --- Folder selection -> AccountCommand::SyncMailbox on that folder's
    // own account (selecting an account-group row, or the Favorites section
    // header, is a no-op - it just expands/collapses). The synthetic "All
    // Inboxes" row instead enters the unified view. A `Favorite` row is the
    // same mailbox as its `Folder` row, so it takes the identical path. Every
    // live path re-renders the list header, which is the one place both view
    // changes funnel through. ---
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        let search_entry = search_entry.clone();
        folder_selection.connect_selected_item_notify(move |sel| {
            // A rebuild putting the highlight back where it was is not the
            // user navigating; see `UiState::suppress_folder_selection`.
            if state.borrow().suppress_folder_selection {
                return;
            }
            let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let tree_item = boxed.borrow::<TreeItem>();
            match &*tree_item {
                TreeItem::Unified(_) => {
                    drop(tree_item);
                    // Clicking a folder is a deliberate navigation away from
                    // search; leave it and let the selection take over.
                    exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, &search_entry);
                    enter_unified_inbox(&state, &message_list, &message_list_stack);
                    refresh_list_header(&state, &list_header);
                }
                TreeItem::Folder(node) | TreeItem::Favorite(node) => {
                    let mailbox_id = node.mailbox.id.clone();
                    let account_id = node.mailbox.account_id.clone();
                    drop(tree_item);
                    exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, &search_entry);
                    select_mailbox(&state, &worker, &message_list, &message_list_stack, account_id, mailbox_id);
                    refresh_list_header(&state, &list_header);
                }
                TreeItem::Account(_) | TreeItem::Favorites | TreeItem::SearchFolders => {}
                TreeItem::SearchFolder(folder) => {
                    let query = folder.query.clone();
                    drop(tree_item);
                    // A saved search runs across every mailbox (item 2.4 /
                    // M-19), the way Outlook's search folders do.
                    state.borrow_mut().search_scope = lookout_mail::SearchScope::All;
                    start_search(&state, &worker, &message_list, &list_header, &query);
                }
            }
        });
    }

    // --- Message list model contents changed -> re-check the loading spinner ---
    // `repopulate`/`repopulate_unified_slice` (message_list.rs) splice a large
    // mailbox's computed layout onto the model asynchronously, off the GTK
    // main thread - by the time that dispatch is even started, the
    // `MessagesUpdated` handler that triggered it has already returned, so
    // nothing there can re-check `refresh_message_loading_state` once the
    // splice actually lands. `items-changed` (from `gio::ListModel`, distinct
    // from `MultiSelection`'s own `selection-changed`) fires synchronously
    // from every splice, sync or async, so hooking it here is the one place
    // that catches both paths without every `repopulate` call site needing to
    // remember to re-check itself.
    {
        let state = state.clone();
        let message_list_for_items_changed = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        message_list.selection.connect_items_changed(move |_model, _pos, _removed, _added| {
            refresh_message_loading_state(&state, &message_list_for_items_changed, &message_list_stack);
        });
    }

    // --- Message selection -> AccountCommand::FetchBody on the current account ---
    {
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        let reading_multi_label = reading_multi_label.clone();
        let message_header = message_header.clone();
        let message_list_for_selection = message_list.clone();
        let mark_read_icon = mark_read_icon.clone();
        let pin_icon = pin_icon.clone();
        let flag_icon = flag_icon.clone();
        let web_view = web_view.clone();
        let user_content_manager = user_content_manager.clone();
        let theme_override_sheet = theme_override_sheet.clone();
        let conversation_context = conversation_context.clone();
        message_list.selection.connect_selection_changed(move |_sel, pos, n_items| {
            let target = message_list_for_selection.reading_target();
            let kind = match &target {
                ReadingTarget::Empty => "Empty",
                ReadingTarget::Single(_) => "Message",
                ReadingTarget::Conversation { .. } => "Conversation",
                ReadingTarget::Multiple(_) => "Multiple",
            };
            tracing::debug!(pos, n_items, kind, "selection-changed fired");
            refresh_mark_read_button(&mark_read_icon, &message_list_for_selection);
            refresh_pin_button(&pin_icon, &message_list_for_selection, &state);
            refresh_flag_button(&flag_icon, &message_list_for_selection);
            let summary = match target {
                ReadingTarget::Single(summary) => {
                    // A lone message (or any message while conversations are
                    // off): leave a conversation that was showing.
                    leave_conversation(&state);
                    *summary
                }
                // A conversation header, or a message inside a listed
                // conversation: the whole conversation (item 1.7).
                ReadingTarget::Conversation { id, messages, focus } => {
                    open_conversation(&conversation_context, id, messages, focus);
                    return;
                }
                ReadingTarget::Multiple(summaries) => {
                    leave_conversation(&state);
                    // Two or more messages selected: show the "N selected"
                    // placeholder and skip mark-as-read/body-fetch entirely -
                    // the same state reset the `Empty` arm below does, since
                    // both mean "the reading pane no longer shows a specific
                    // message." Leaving `rendered_message` set here would
                    // make a later re-selection of that same single message
                    // look already-shown and skip re-rendering it. The
                    // message-theme override resets to the configured default
                    // (Config → Appearance → "Dark message theme") and its
                    // physical sheet/canvas are re-armed to match.
                    // Leaving the single message that was showing marks it
                    // read in "on-selection-change" mode (item 1.8).
                    flush_mark_read_on_leave(&state);
                    cancel_pending_mark_read(&state);
                    let default_dark = {
                        let mut st = state.borrow_mut();
                        st.pending_body_request = None;
                        st.body_fetch_error = None;
                        st.body_fetch_generation += 1;
                        st.pending_html_reveal = false;
                        st.reveal_generation += 1;
                        st.pending_header = None;
                        st.pending_attachment = None;
                        st.pending_raw_message = None;
                        st.unsubscribe_info = None;
                        st.unsubscribe_dismissed = None;
                        st.imip = None;
                        st.imip_dismissed = None;
                        st.read_receipt_request = None;
                        st.read_receipt_dismissed = None;
                        st.read_receipt_context = None;
                        st.rendered_trust_sender = None;
                        st.load_once_images = false;
                        st.message_theme_override = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                        st.trust_banner_dismissed = None;
                        st.rendered_message = None;
                        st.rendered_draft = None;
                        st.rendered_snooze = None;
                        st.rendered_follow_up = None;
                        st.pending_draft_open = None;
                        st.message_theme_override
                    };
                    drop_pending_cid(&state);
                    set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
                    stop_body_loading(&reading_stack);
                    reading_multi_label.set_label(&format!("{} messages selected", summaries.len()));
                    reading_stack.set_visible_child_name("multi");
                    return;
                }
                ReadingTarget::Empty => {
                    // Nothing selected: show the "no message selected"
                    // placeholder and drop every per-message state - but only
                    // once this is confirmed to persist. A list repopulate's
                    // splice can transiently shrink the selection to nothing
                    // before `restore_selection` (moments later, same
                    // synchronous call) puts it right back on the same
                    // message - acting on that blip immediately would clear
                    // `pending_body_request` before the `Message` arm's
                    // re-fire ever gets a chance to recognize it as the same
                    // request (see that arm's same-request guard), stranding
                    // any fetch or WebKit load already in flight for it.
                    // Deferring to the next main-loop idle callback lets that
                    // resettle first; the reset below only actually applies
                    // if the selection is still empty by then.
                    let state = state.clone();
                    let message_list_for_selection = message_list_for_selection.clone();
                    let web_view = web_view.clone();
                    let user_content_manager = user_content_manager.clone();
                    let theme_override_sheet = theme_override_sheet.clone();
                    let reading_stack = reading_stack.clone();
                    glib::idle_add_local_once(move || {
                        if !matches!(message_list_for_selection.selection_kind(), SelectionKind::Empty) {
                            return;
                        }
                        leave_conversation(&state);
                        // The pane is genuinely empty now: mark the message we
                        // just left read in "on-selection-change" mode (item
                        // 1.8) and drop any delayed mark queued for it.
                        flush_mark_read_on_leave(&state);
                        cancel_pending_mark_read(&state);
                        let default_dark = {
                            let mut st = state.borrow_mut();
                            st.pending_body_request = None;
                            st.body_fetch_error = None;
                            st.body_fetch_generation += 1;
                            st.pending_html_reveal = false;
                            st.reveal_generation += 1;
                            st.pending_header = None;
                            st.pending_attachment = None;
                            st.pending_raw_message = None;
                            st.unsubscribe_info = None;
                            st.unsubscribe_dismissed = None;
                            st.imip = None;
                            st.imip_dismissed = None;
                            st.read_receipt_request = None;
                            st.read_receipt_dismissed = None;
                            st.read_receipt_context = None;
                            st.rendered_trust_sender = None;
                            st.load_once_images = false;
                            st.message_theme_override = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                            st.trust_banner_dismissed = None;
                            st.rendered_message = None;
                            st.rendered_draft = None;
                            st.rendered_snooze = None;
                            st.rendered_follow_up = None;
                            st.pending_draft_open = None;
                            st.message_theme_override
                        };
                        drop_pending_cid(&state);
                        set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
                        stop_body_loading(&reading_stack);
                        reading_stack.set_visible_child_name("empty");
                    });
                    return;
                }
            };
            let uid = summary.uid;
            let mailbox = summary.mailbox.clone();
            // A draft-open request (item 1.12) interacts with selection in two
            // ways: re-selecting the message whose raw bytes are still in
            // flight must leave the loading page alone (a repaint re-firing
            // this handler must not restart the body fetch over it), and
            // moving to a different message abandons the request - its late
            // response is then dropped by `RawMessageFetched`.
            {
                let mut st = state.borrow_mut();
                match st.pending_draft_open.as_ref() {
                    Some(pending) if pending.mailbox == mailbox && pending.uid == uid => return,
                    Some(_) => st.pending_draft_open = None,
                    None => {}
                }
            }
            // Re-selecting the message that's already on the reading pane -
            // which `restore_selection` can do across a rebuild that
            // preserves the same selected row - must be a no-op, not a fresh
            // fetch/render. Routing it through "empty" and crossfading the
            // same email back in would be a startup flicker; the body is
            // already on screen, so there's nothing to re-render.
            let already_shown = {
                let st = state.borrow();
                st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid)) && reading_stack.visible_child_name().as_deref() == Some("message")
            };
            if already_shown {
                return;
            }
            // The selection has moved off whatever was showing: in
            // "on-selection-change" mode this is when the previous message
            // becomes read (item 1.8), and any delayed mark queued for it is
            // dropped.
            flush_mark_read_on_leave(&state);
            cancel_pending_mark_read(&state);
            let mark_read_mode = crate::settings::mark_as_read_mode(&state.borrow().settings.get_string(crate::settings::MARK_AS_READ_MODE));
            let request = (mailbox.clone(), uid);
            // Opening a message is what marks it read - bodies are fetched
            // with `BODY.PEEK`, so the server never sets `\Seen` itself (see
            // `fetch_body` in lookout-mail). Sent past the `already_shown`
            // guard above, so the rebuild this triggers can't loop: the
            // message comes back with `\Seen` set, and a re-selection of the
            // row already on the reading pane returns before reaching here.
            // The mark-as-read setting decides *when* it happens (item 1.8).
            let mark_read = summary.is_unread();
            // Deferred to the reveal: updating the header here would swap it
            // to the next email while the previous message is still fading
            // out. `render_body` applies it once the new body is about to be
            // shown (the pane is on "empty" by then, so it can't flash).
            state.borrow_mut().pending_header = Some(summary);
            if mark_read {
                match mark_read_mode {
                    crate::settings::MarkAsReadMode::WhenOpened => mark_message_read(&state, &mailbox, uid),
                    crate::settings::MarkAsReadMode::AfterDelay => {
                        let delay = crate::settings::mark_as_read_delay(state.borrow().settings.get_int(crate::settings::MARK_AS_READ_DELAY));
                        arm_pending_mark_read(&state, &message_list_for_selection, &mailbox, uid, delay as u32);
                    }
                    crate::settings::MarkAsReadMode::OnSelectionChange => {
                        state.borrow_mut().mark_read_on_leave = Some((mailbox.clone(), uid));
                    }
                    crate::settings::MarkAsReadMode::Never => {}
                }
            }

            let body_is_cached = state.borrow().body_cache.contains(&mailbox, &uid);
            let should_request = {
                let mut st = state.borrow_mut();
                let is_same_request = st.pending_body_request.as_ref() == Some(&request);
                // Disarm any body load still in flight for the previously
                // selected message, so its `Finished` can't reveal a stale
                // email once the user has moved on. The reveal-fallback
                // timeouts capture `reveal_generation` at arm time, so the
                // bump here also invalidates any timeout from an earlier
                // selection whose load hasn't finished yet.
                //
                // Only when the selection is actually changing to a
                // *different* message, though: this handler can re-fire for
                // the message that's already selected - a list repaint
                // (e.g. the resync that follows marking this very message
                // read) can transiently touch its row during a splice, which
                // `restore_selection` corrects moments later, but not before
                // this signal fires again for the same `(mailbox, uid)`.
                // Disarming on that re-fire would strand the pane on "empty"
                // forever - WebKit only fires `Finished` once per load, so a
                // load that's still genuinely in flight for this exact
                // message must not be cancelled here.
                if !is_same_request {
                    st.pending_html_reveal = false;
                    st.reveal_generation += 1;
                    st.pending_body_request = Some(request.clone());
                }
                !body_is_cached && !is_same_request
            };
            if should_request {
                // `request_body_fetch` sends the command, arms the failure
                // backstop, shows the loading page, and starts the parallel
                // disk-cache read.
                request_body_fetch(&state, &worker, &reading_stack, &message_header, mailbox.clone(), uid);
            }
            // Also silently abandons an in-progress composer in the reading
            // pane, if one's open - no "discard draft?" prompt, consistent
            // with this app's existing no-confirmation-dialog convention.
            // Every navigation resets the per-message override to the
            // configured default (Config → Appearance → "Dark message
            // theme"), so the next message opens in that default rather than
            // inheriting the previous message's manual toggle - the physical
            // sheet/canvas are re-armed to match before the new body renders
            // (or while the pane sits on "loading" waiting for a fetch).
            {
                let mut st = state.borrow_mut();
                let default_dark = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                st.message_theme_override = default_dark;
                drop(st);
                set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
            }
            if body_is_cached {
                // Route through the blank page so the new body crossfades in
                // rather than swapping in place.
                reading_stack.set_visible_child_name("empty");
                let body = state.borrow_mut().body_cache.get(&mailbox, &uid);
                if let Some(body) = body {
                    tracing::debug!(?mailbox, uid = uid.0, "FetchBody: serving from in-memory cache");
                    render_body(&state, &reading_stack, &message_header, mailbox, uid, body);
                }
            }
        });
    }

    // --- Delete/Archive/Report -> AccountCommand::MoveMessages against the
    // account's Trash/Archive/Junk mailbox (Snooze has its own preset menu,
    // below). All three are silent no-ops
    // with nothing selected, and send exactly one command per distinct
    // mailbox in the selection (see `command_targets_for`) -
    // for today's ordinary single-selection case that's exactly one command,
    // same as before.
    for (button, role) in [(&delete_button, MailboxRole::Trash), (&archive_button, MailboxRole::Archive)] {
        let message_list = message_list.clone();
        let state = state.clone();
        button.connect_clicked(move |_| move_messages_to_role(&state, &message_list, &message_list.selected_summaries(), role));
    }
    // Report junk also sets the RFC 5788 `$Junk` keyword, so it goes through
    // `set_junk_selection` rather than the plain role move (item 1.24).
    {
        let message_list = message_list.clone();
        let state = state.clone();
        report_button.connect_clicked(move |_| set_junk_selection(&state, &message_list, &message_list.selected_summaries(), true));
    }
    // --- Pin/Unpin -> AccountCommand::StoreFlagsMany toggling `\Flagged`
    // on every selected message. The direction is computed once over the
    // whole selection - Gmail/Outlook's convention: any unpinned message
    // selected means the action pins everything; only when every selected
    // message is already pinned does it become "unpin all" - rather than
    // per-message, so a single selected message (the common case) sees
    // exactly today's is_pinned()-based toggle.
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let pin_icon_for_click = pin_icon.clone();
        pin_button.connect_clicked(move |_| {
            let summaries = message_list.selected_summaries();
            let pinning = summaries.iter().any(|s| !s.is_pinned());
            set_messages_pinned(&state, &message_list, &summaries, pinning);
            // The optimistic patch above already moved every affected
            // message into/out of the Pinned group and restored the
            // selection, so the aggregate state `refresh_pin_button` reads
            // off `selected_summaries()` is genuinely current - no need for
            // a separate manual icon flip first.
            refresh_pin_button(&pin_icon_for_click, &message_list, &state);
        });
    }
    // --- Flag ▾ (item 1.16): the main part toggles the selection's flag,
    // the arrow opens the due-date menu.
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let flag_icon_for_click = flag_icon.clone();
        flag_button.connect_clicked(move |_| {
            toggle_follow_up(&state, &message_list, message_list.selected_summaries());
            refresh_flag_button(&flag_icon_for_click, &message_list);
        });
    }
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let state_for_pick = state.clone();
        let flag_icon_for_pick = flag_icon.clone();
        flag_button.set_popover(Some(&build_flag_popover(
            &state,
            Rc::new(move |flag| {
                set_follow_up(&state_for_pick, &message_list, message_list.selected_summaries(), flag);
                refresh_flag_button(&flag_icon_for_pick, &message_list);
            }),
        )));
    }
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let mark_read_icon_for_click = mark_read_icon.clone();
        mark_read_button.connect_clicked(move |_| {
            let summaries = message_list.selected_summaries();
            let mark_read = summaries.iter().any(|s| s.is_unread());
            set_messages_read(&state, &message_list, &summaries, mark_read);
            refresh_mark_read_button(&mark_read_icon_for_click, &message_list);
        });
    }
    // --- Snooze ▾ (item 1.15): the button opens Outlook's preset menu (Later
    // today, Tomorrow, This weekend, Next week, Choose a date…). It stays a
    // plain button so the Snooze shortcut's `emit_clicked` opens the menu too.
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = crate::datetime_picker::build_time_preset_popover(
            "Snooze until",
            Rc::new(|now| {
                crate::snooze::snooze_presets(&now)
                    .into_iter()
                    .map(|preset| crate::datetime_picker::TimeChoice {
                        name: preset.name.to_string(),
                        at: preset.at,
                    })
                    .collect()
            }),
            "Choose a date…",
            snooze_time_dialog(),
            Rc::new(move |until| snooze_selection(&state, &message_list, until)),
        );
        popover.set_parent(&snooze_button);
        {
            let popover = popover.clone();
            snooze_button.connect_clicked(move |_| popover.popup());
        }
        snooze_button.connect_destroy(move |_| popover.unparent());
    }

    // --- Main calendar navigation: prev/next/Today move the anchor by the
    // active view's natural unit (day/week/month - see `calendar_view::step`)
    // and redraw every view immediately, then ask every connected calendar
    // account to resync the newly-displayed month.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let prev_button = calendar_main.prev_button.clone();
        prev_button.connect_clicked(move |_| {
            calendar_view::step(&calendar_main, -1);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let next_button = calendar_main.next_button.clone();
        next_button.connect_clicked(move |_| {
            calendar_view::step(&calendar_main, 1);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let today_button = calendar_main.today_button.clone();
        today_button.connect_clicked(move |_| {
            calendar_view::go_today(&calendar_main);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Sidebar mini-calendar -> re-anchor the main panel to the clicked
    // date (whatever view is active) and resync that date's month.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        calendar_view::connect_day_selected(&calendar_sidebar.mini_calendar, move |date| {
            calendar_view::set_anchor(&calendar_main, date);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Header current-month dropdown -> the same re-anchor as the sidebar
    // mini-calendar: clicking a day in the dropdown's mini grid navigates the
    // whole panel there (and `build_main` closes the popover itself on pick).
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let month_mini = calendar_main.month_mini.clone();
        calendar_view::connect_day_selected(&month_mini, move |date| {
            calendar_view::set_anchor(&calendar_main, date);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Mail-screen overview mini-calendar -> re-show that day's events
    // (from whatever's already cached) and ask every connected calendar
    // account to resync that month in the background, without touching the
    // main Calendar view's own `displayed_month`.
    {
        let calendar_state = calendar_state.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        calendar_view::connect_day_selected(&mail_calendar_overview, move |date| {
            mail_overview_day.set(date);
            refresh_mail_overview_day_list(&calendar_state, date, &mail_overview_day_list);
            let month = first_of_month(date);
            for handle in calendar_state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
            }
            let webcal_cmd = calendar_state.borrow().webcal_cmd_tx.clone();
            if let Some(cmd) = webcal_cmd {
                let _ = cmd.send_blocking(SubscriptionCommand::SyncMonth(month));
            }
        });
    }

    // --- Calendar event editor: the "New Event" toolbar button opens a blank
    // form prefilled for the displayed date (and the Day/Week grids' empty
    // time slots open one prefilled for the clicked slot - see the
    // `connect_slot_activated` block below); clicking an event in any calendar
    // view opens it for editing/deleting. Both save and delete hand the
    // result to the owning account's session (`route_calendar_save`/`_delete`),
    // which PUTs/DELETEs it and resyncs the month - the UI repaints through the
    // existing `OccurrencesUpdated` path, so there's no refresh work here.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        new_event_button.connect_clicked(move |_| {
            let anchor = calendar_view::anchor(&calendar_main);
            let suggested_start = {
                let now = chrono::Local::now();
                if now.date_naive() == anchor {
                    if now.hour() == 23 {
                        (anchor + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap()
                    } else {
                        anchor.and_hms_opt(now.hour() + 1, 0, 0).unwrap()
                    }
                } else {
                    anchor.and_hms_opt(9, 0, 0).unwrap()
                }
            };
            show_new_event_editor(&window, &state, &worker, &calendar_state, &toast_overlay, suggested_start, None, &[], None, &profile_card);
        });
    }
    // Print: snapshots the currently-displayed month (checked calendars
    // only, matching the view) as an agenda and sends it through WebKit's
    // print pipeline.
    {
        let window = window.clone();
        let calendar_state = calendar_state.clone();
        print_button.connect_clicked(move |_| {
            print_calendar_month(&calendar_state, &window);
        });
    }
    // --- Task editor + list wiring: the "New Task" toolbar button opens a
    // blank form, clicking a task row opens it for editing, and the row
    // checkbox flips completion (the modified task goes through the same
    // session route as a Save). Opening the Tasks view pulls a fresh task
    // list from every source so the view isn't stale between polls.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        new_task_button.connect_clicked(move |_| {
            show_new_task_editor(&window, &state, &calendar_state, &tasks_view);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_state_for_activate = calendar_state.clone();
        let tasks_view_for_toggle = tasks_view.clone();
        let tasks_view_for_activate = tasks_view.clone();
        let toast_overlay_for_toggle = toast_overlay.clone();
        crate::tasks_view::set_handlers(
            &tasks_view,
            Rc::new(move |previous, task, completed| route_task_toggle(&calendar_state, &tasks_view_for_toggle, &toast_overlay_for_toggle, previous, task, completed)),
            Rc::new(move |task| open_task_editor_for(&window, &state, &calendar_state_for_activate, &tasks_view_for_activate, &task)),
        );
    }
    // Tasks navigation pane (item 4.8 / T-01): selecting a row switches the
    // filter, and the list rows' create/rename/delete actions route to the
    // owning backend via `task_lists`.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view_for_nav = tasks_view.clone();
        let toast_overlay = toast_overlay.clone();
        crate::tasks_view::set_nav_handler(
            &tasks_view,
            Rc::new(move |action| {
                use crate::tasks_view::TasksNavAction;
                match action {
                    TasksNavAction::Select(filter) => {
                        if calendar_state.borrow().tasks_filter != filter {
                            calendar_state.borrow_mut().tasks_filter = filter;
                            refresh_tasks_view(&calendar_state, &tasks_view_for_nav);
                        }
                    }
                    TasksNavAction::NewList => {
                        crate::task_lists::create_task_list_dialog(&window, &state, &calendar_state, &tasks_view_for_nav, &toast_overlay);
                    }
                    TasksNavAction::RenameList(id) => {
                        crate::task_lists::rename_task_list_dialog(&window, &state, &calendar_state, &tasks_view_for_nav, &toast_overlay, id);
                    }
                    TasksNavAction::DeleteList(id) => {
                        crate::task_lists::delete_task_list_confirm(&window, &state, &calendar_state, &tasks_view_for_nav, &toast_overlay, id);
                    }
                }
            }),
        );
    }
    // My Day (item 4.9 / T-02): the Add/Remove action flips the pin set,
    // persists it to the UI-state database and repaints.
    {
        let calendar_state = calendar_state.clone();
        let tasks_view_for_my_day = tasks_view.clone();
        crate::tasks_view::set_my_day_handler(
            &tasks_view,
            Rc::new(move |key| {
                let day = calendar_state.borrow().my_day_day.format("%Y-%m-%d").to_string();
                let pins = {
                    let mut st = calendar_state.borrow_mut();
                    crate::my_day::toggle(&mut st.my_day_pins, key);
                    st.my_day_pins.clone()
                };
                if let Some(db) = calendar_state.borrow().local_tasks_db.clone() {
                    let _ = db.borrow().set_my_day(&day, &pins);
                }
                refresh_tasks_view(&calendar_state, &tasks_view_for_my_day);
            }),
        );
    }
    // Tasks view arrangement (T-09): seed the view's options from the
    // persisted settings, then keep them in sync from the toolbar menu.
    // Reading all three back from settings after each write keeps the setters
    // free of partial-update bookkeeping.
    let apply_task_options = {
        let state = state.clone();
        let tasks_view = tasks_view.clone();
        Rc::new(move || {
            let (sort, group, show) = {
                let st = state.borrow();
                (
                    st.settings.get_string(crate::settings::TASKS_SORT_KEY),
                    st.settings.get_string(crate::settings::TASKS_GROUP_BY),
                    st.settings.get_bool(crate::settings::TASKS_SHOW_COMPLETED),
                )
            };
            crate::tasks_view::set_view_options(
                &tasks_view,
                crate::tasks_view::TaskSortKey::from_setting(&sort),
                crate::tasks_view::TaskGroupBy::from_setting(&group),
                show,
            );
        })
    };
    apply_task_options();
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let apply_task_options = apply_task_options.clone();
        task_sort_action.connect_activate(move |action, parameter| {
            let Some(value) = parameter.and_then(|p| p.get::<String>()) else { return };
            action.set_state(&value.to_variant());
            state.borrow().settings.set_string(crate::settings::TASKS_SORT_KEY, &value);
            apply_task_options();
            refresh_tasks_view(&calendar_state, &tasks_view);
        });
    }
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let apply_task_options = apply_task_options.clone();
        task_group_action.connect_activate(move |action, parameter| {
            let Some(value) = parameter.and_then(|p| p.get::<String>()) else { return };
            action.set_state(&value.to_variant());
            state.borrow().settings.set_string(crate::settings::TASKS_GROUP_BY, &value);
            apply_task_options();
            refresh_tasks_view(&calendar_state, &tasks_view);
        });
    }
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let apply_task_options = apply_task_options.clone();
        // A boolean stateful action renders as a check item and is toggled
        // through `change-state`, not `activate` (same as the mail filters).
        task_show_completed_action.connect_change_state(move |action, requested| {
            let on = requested.and_then(|value| value.get::<bool>()).unwrap_or(false);
            action.set_state(&on.to_variant());
            state.borrow().settings.set_bool(crate::settings::TASKS_SHOW_COMPLETED, on);
            apply_task_options();
            refresh_tasks_view(&calendar_state, &tasks_view);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        tasks_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                refresh_tasks_view(&calendar_state, &tasks_view);
                for handle in calendar_state.borrow().accounts.values() {
                    let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncTasks);
                }
                for handle in calendar_state.borrow().google_tasks.values() {
                    let _ = handle.cmd_tx.send_blocking(GoogleTasksCommand::Refresh);
                }
                for handle in calendar_state.borrow().graph_todo.values() {
                    let _ = handle.cmd_tx.send_blocking(lookout_graph::TodoCommand::Refresh);
                }
            }
        });
    }
    // --- Lookout dashboard wiring: its task rows route through the same
    // session paths as the Tasks view (so a completion or edit lands on the
    // owning store), the tab open repaints + widens the sync horizon, and
    // the toolbar's Refresh does the same on demand. The dashboard's own
    // repaint on task changes piggybacks on `refresh_tasks_view`, which
    // every save path funnels through.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let calendar_state_for_activate = calendar_state.clone();
        let tasks_view_for_activate = tasks_view.clone();
        let toast_overlay_for_toggle = toast_overlay.clone();
        crate::lookout_view::set_handlers(
            &lookout_view,
            Rc::new(move |previous, task, completed| route_task_toggle(&calendar_state, &tasks_view, &toast_overlay_for_toggle, previous, task, completed)),
            Rc::new(move |task| open_task_editor_for(&window, &state, &calendar_state_for_activate, &tasks_view_for_activate, &task)),
        );
    }
    {
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let lookout_view = lookout_view.clone();
        lookout_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                // Bypasses `dashboard_refresh`'s visibility gate/debounce -
                // the dashboard becoming visible is exactly the moment it
                // must repaint immediately, not up to 500ms later.
                refresh_lookout_view(&state, &worker, &calendar_state, &lookout_view);
                widen_calendar_sync_horizon(&calendar_state);
            }
        });
    }
    {
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let lookout_view = lookout_view.clone();
        lookout_refresh_button.connect_clicked(move |_| {
            // Bypasses the debounce for the same reason: an explicit click
            // asking for a refresh should never wait out the trailing edge.
            refresh_lookout_view(&state, &worker, &calendar_state, &lookout_view);
            widen_calendar_sync_horizon(&calendar_state);
        });
    }
    {
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let toast_overlay = toast_overlay.clone();
        connect_google_tasks_button.connect_clicked(move |_| {
            connect_google_tasks(&worker, &calendar_state, &tasks_view, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        calendar_view::connect_event_activated(&calendar_main, move |occ| {
            open_event_editor_for(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ);
        });
    }
    // The calendar event loop's way back into an active event search: the
    // loop has no `Worker`/results-page references, so it calls this hook
    // after a month sync or an event edit (CAL-22).
    calendar_state.borrow_mut().calendar_search_refresh = Some({
        let calendar_state = calendar_state.clone();
        let worker = worker.clone();
        let calendar_main = calendar_main.clone();
        // The backfill and a batch of month syncs can fire dozens of these in
        // quick succession; coalesce them so the cache isn't queried once per
        // month.
        let generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        Rc::new(move || {
            let token = generation.get().wrapping_add(1);
            generation.set(token);
            let calendar_state = calendar_state.clone();
            let worker = worker.clone();
            let calendar_main = calendar_main.clone();
            let generation = generation.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(750), move || {
                if generation.get() == token {
                    rerun_calendar_search(&calendar_state, &worker, &calendar_main);
                }
            });
        })
    });
    // The calendar event loop's window into the event editor: after a
    // context-menu Duplicate / Copy (item 3.12) the loop opens the new
    // occurrence's editor once the server has confirmed it.
    calendar_state.borrow_mut().open_event_editor = Some({
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        Rc::new(move |occ| {
            open_event_editor_for(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ);
        })
    });
    // --- Calendar event context menu (item 3.12 / CAL-31): a right-click on
    // an event in any view opens Outlook's event menu (Open, Delete,
    // Duplicate, Copy to calendar…, Save as .ics…, Categorize, Show as,
    // Private). The menu's effects run through the same helpers as the editor,
    // so a duplicate prompts for invitations exactly like a save would.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        calendar_view::connect_event_context_menu(&calendar_main, move |occ, anchor, point| {
            show_event_menu(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ, &anchor, point);
        });
    }
    // --- Calendar event peek (UX-56): a single click on an event chip in the
    // Month/Split grids shows a compact read-only card anchored to the chip,
    // with Edit (the full editor), Delete and Join actions. A double-click
    // still routes through `connect_event_activated` above.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        calendar_view::connect_event_peeked(&calendar_main, move |occ, anchor| {
            show_event_peek(&window, &state, &worker, &calendar_state, &toast_overlay, &profile_card, &occ, &anchor);
        });
    }
    // --- Calendar drag-reschedule: dropping an event chip at a new position
    // in the Day/Week/Work week or Month/Split grids persists the change
    // through the same route the editor's save uses (an etag-guarded
    // `UpdateEvent`, resync on success, error toast on failure). Webcal
    // subscriptions have no write-back path, so their events can't move. A
    // recurring occurrence's drag lands as a per-occurrence override ("This
    // occurrence" scope): the instance moves without re-anchoring the series.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main_for_drag = calendar_main.clone();
        let toast_overlay = toast_overlay.clone();
        calendar_view::connect_event_dragged(&calendar_main, move |occ, new_start, new_end, copy| {
            if calendar_state.borrow().is_read_only_calendar(&occ.calendar_id) {
                toast_overlay.add_toast(adw::Toast::new("Events from calendar subscriptions are read-only."));
                return;
            }
            // Ctrl-drag copies the occurrence to the dropped time instead of
            // moving it (CAL-30 / item 3.12): a brand-new event in the same
            // calendar, so no move is persisted and no editor opens.
            if copy {
                create_event_copy(
                    &window,
                    &state,
                    &calendar_state,
                    &toast_overlay,
                    &occ,
                    occ.calendar_id.clone(),
                    Some((new_start, new_end)),
                    false,
                    "Event copied",
                );
                return;
            }
            // Patch the dropped occurrence into the on-screen model and
            // repaint immediately, before the CalDAV round trip even starts -
            // otherwise the chip snaps back to its pre-drag position the
            // instant the drag gesture's own ghost-chip repaint runs, and
            // stays there until the server confirms the move. The repaint
            // itself is deferred one main-loop idle tick: this callback runs
            // while the time grid's drag gesture still holds a borrow on its
            // own `on_drag` slot to invoke it, and `set_time_grid` (which the
            // repaint reaches) re-borrows that same slot - calling it
            // synchronously here panics with "already borrowed". The defer
            // is a same-frame, imperceptible delay, not the round-trip wait
            // this fix removes.
            let mut patched = occ.clone();
            patched.start = new_start;
            patched.end = new_end;
            calendar_state.borrow_mut().pending_calendar_moves.insert((occ.uid.clone(), occ.recurrence_id), patched);
            {
                let calendar_state = calendar_state.clone();
                let calendar_main_for_drag = calendar_main_for_drag.clone();
                glib::idle_add_local_once(move || {
                    refresh_displayed_calendar_view(&calendar_state, &calendar_main_for_drag);
                });
            }
            let (start_local, end_local) = crate::event_editor::dragged_local_span(&occ, new_start, new_end);
            let mut event = crate::event_editor::calendar_event_from_occurrence(&occ, start_local, end_local);
            crate::event_editor::apply_edit_scope(&mut event, 0, Some(&occ));
            // A drag is an edit: it bumps SEQUENCE and offers the "send
            // update?" prompt, exactly like a save from the editor.
            save_event_with_invites(&window, &state, &calendar_state, &toast_overlay, Some(&occ), event.calendar_id.clone(), event);
        });
    }
    // --- Calendar main grid interactions: the first click on an empty time
    // slot in the Day/Week grids (or on a month-grid day cell) selects and
    // highlights it; a second click on the highlighted slot/day opens the
    // new-event editor prefilled for that exact time. Clicking a day cell in
    // the Month/Split grid re-anchors every view to it as it selects - the
    // large grid's counterpart of the sidebar mini-calendar's day buttons.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        calendar_view::connect_slot_activated(&calendar_main, move |start_date, start_minutes, end_date, end_minutes| {
            let Some(start) = start_date.and_hms_opt((start_minutes / 60) as u32, (start_minutes % 60) as u32, 0) else {
                return;
            };
            // The selection covers through the end of its last slot: a
            // 9:00-11:00 drag is a 2h event ending 11:30 (the last 30 minutes
            // fill out the highlighted slot). A single-slot click passes no
            // end, so the editor's usual one-hour default applies.
            let end = if end_date == start_date && end_minutes == start_minutes {
                None
            } else if end_minutes + 30 >= 1440 {
                (end_date + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)
            } else {
                end_date.and_hms_opt(((end_minutes + 30) / 60) as u32, ((end_minutes + 30) % 60) as u32, 0)
            };
            show_new_event_editor(&window, &state, &worker, &calendar_state, &toast_overlay, start, end, &[], None, &profile_card);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        calendar_view::connect_main_day_selected(&calendar_main, {
            let calendar_main = calendar_main.clone();
            move |date| {
                calendar_view::set_anchor(&calendar_main, date);
                show_anchor(&calendar_state, &calendar_main, &mini_calendar);
            }
        });
    }
    // Clicking a month-grid day cell a second time (it's already highlighted)
    // opens the new-event editor prefilled for 9am of that day.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        let profile_card = profile_card_provider.clone();
        calendar_view::connect_main_day_activated(&calendar_main, move |date| {
            let Some(start) = date.and_hms_opt(9, 0, 0) else {
                return;
            };
            show_new_event_editor(&window, &state, &worker, &calendar_state, &toast_overlay, start, None, &[], None, &profile_card);
        });
    }

    // --- Calendar sidebar "Add calendar" -> the subscribe/import/manage
    // dialog (webcal feeds + .ics import). Feeds are read-only, so the dialog
    // is also where they get removed again.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_sidebar.calendar_list_box.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let toast_overlay = toast_overlay.clone();
        calendar_sidebar.add_calendar_button.connect_clicked(move |_| {
            show_calendar_dialog(
                &window,
                &state,
                &calendar_state,
                &calendar_main,
                &calendar_list_box,
                &mini_calendar,
                &mail_overview_day,
                &mail_overview_day_list,
                &toast_overlay,
            );
        });
    }

    // --- Calendar sidebar row/group actions (item 3.13 / CAL-32, CAL-33):
    // the per-calendar ⋯ menu (Change colour, Rename, Share, Remove, Move to
    // group) and the group header/footer actions (New/Rename/Delete group,
    // Show all). Registered on `calendar_state` (the late-binding hook
    // pattern) so every checklist rebuild can reach it.
    calendar_glue::install_calendar_sidebar_actions(
        &window,
        &state,
        &calendar_state,
        &calendar_main,
        &calendar_sidebar.calendar_list_box,
        &calendar_sidebar.mini_calendar,
        &mail_overview_day,
        &mail_overview_day_list,
        &tasks_view,
        &toast_overlay,
    );

    // --- Calendar Home ribbon "Share" -> the calendar-sharing dialog for the
    // user's Microsoft 365 calendar (item 5.6 / CAL-24).
    {
        let window = window.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        share_button
            .clone()
            .connect_clicked(move |_| match calendar_glue::first_shareable_calendar(&calendar_state) {
                Some(calendar) => crate::calendar_share::show_share_calendar_dialog(&window, calendar, &toast_overlay),
                None => toast_overlay.add_toast(adw::Toast::new("Sharing is available for Microsoft 365 calendars")),
            });
    }

    // --- Webcal feed session: one poller for every configured subscription,
    // started independently of GOA discovery (feeds aren't GOA accounts), so
    // subscriptions load and refresh even with no CalDAV account connected.
    spawn_webcal_session(
        worker.clone(),
        calendar_state.clone(),
        state.borrow().app_config.clone(),
        calendar_main.clone(),
        calendar_sidebar.calendar_list_box.clone(),
        calendar_sidebar.mini_calendar.clone(),
        mail_overview_day.clone(),
        mail_overview_day_list.clone(),
        toast_overlay.clone(),
    );

    // Manually-added ("other") IMAP/SMTP accounts - see `other_accounts.rs`.
    // Unlike GOA there's no D-Bus round trip needed to know the list (it's
    // already persisted in `settings.json`), so these connect synchronously,
    // before `spawn_account_discovery` below kicks off GOA's async discovery.
    // `spawn_account_discovery`'s own "no accounts at all" empty-state check
    // accounts for accounts already connected here.
    {
        let other_accounts = state.borrow().app_config.borrow().other_accounts.clone();
        if !other_accounts.is_empty() {
            for account in other_accounts {
                connect_other_account(
                    worker.clone(),
                    state.clone(),
                    folder_selection.clone(),
                    folder_scroller.clone(),
                    message_list.clone(),
                    message_list_stack.clone(),
                    message_header.clone(),
                    reading_stack.clone(),
                    toast_overlay.clone(),
                    window.clone(),
                    app.clone(),
                    list_header.clone(),
                    account,
                    keyring.clone(),
                    dashboard_refresh.clone(),
                    mark_read_icon.clone(),
                    pin_icon.clone(),
                );
            }
            refresh_config();
        }
    }

    spawn_account_discovery(
        worker.clone(),
        state.clone(),
        root_stack.clone(),
        toast_overlay.clone(),
        window.clone(),
        app.clone(),
        folder_selection,
        folder_scroller.clone(),
        message_list,
        message_list_stack,
        message_header,
        reading_stack,
        current_mail_page,
        mail_view_button,
        list_header,
        // Through `refresh_hook`, so discovering GOA accounts also re-checks
        // whether Evolution has anything left to import.
        {
            let refresh_hook = refresh_hook.clone();
            Rc::new(move || {
                let refresh = refresh_hook.borrow().clone();
                refresh();
            })
        },
        current_lookout_page.clone(),
        lookout_view_button.clone(),
        dashboard_refresh.clone(),
        mark_read_icon.clone(),
        pin_icon.clone(),
    );
    spawn_contacts_discovery(
        worker.clone(),
        state.clone(),
        root_stack.clone(),
        toast_overlay.clone(),
        current_contacts_page,
        contacts_view_button,
        refresh_contacts_ui,
        refresh_config.clone(),
    );
    spawn_google_tasks_discovery(worker.clone(), state.clone(), calendar_state.clone(), tasks_view.clone(), toast_overlay.clone());
    spawn_calendar_discovery(
        worker,
        state,
        calendar_state,
        root_stack,
        toast_overlay,
        calendar_main,
        calendar_sidebar.calendar_list_box,
        calendar_sidebar.mini_calendar,
        mail_overview_day,
        mail_overview_day_list,
        current_calendar_page,
        calendar_view_button,
        current_tasks_page,
        tasks_view_button,
        tasks_view,
        reminders_engine,
        refresh_config,
        current_lookout_page,
        lookout_view_button,
    );

    window
}

/// A label-plus-switch row for the People Filter/Sort popover (UX-65).
fn contacts_filter_switch_row(label: &str, switch: &gtk::Switch) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();
    crate::privacy::mark_ui(&text);
    row.append(&text);
    row.append(switch);
    row
}
