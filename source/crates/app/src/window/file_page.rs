//! The File page - Outlook's "Backstage".
//!
//! `File` is not a menu: selecting it replaces the whole window body with
//! this page, which is what Outlook does and what `examples/mailcraft`'s
//! Backstage does. A fixed navigation rail on the left carries `Back` at the
//! top and one row per page, with `Quit Lookout` pinned at the foot; the
//! content pane on the right shows the selected page in a centred,
//! scrollable column of headings, section labels and wide icon tiles.
//!
//! The pages are honest about what Lookout can do today:
//!
//! - **Info** - the connected accounts (from [`UiState::accounts`]), their
//!   servers, a mailbox summary, and the entry points to the Rules and
//!   Categories managers.
//! - **Open & Export** - the import/export flows that exist (Evolution
//!   account import, vCard/CSV contact import and export).
//! - **Options** - Lookout's Settings view (`config_view`), rebuilt in this
//!   page's style: a category pane on the left and the selected category's
//!   settings on the right, with no separate dialog. Opening Options from
//!   anywhere (the header gear, Ctrl+6, an account card, the composer's
//!   signature/template hooks) shows this page on the right category.
//! - **About** - the same facts as Help → About ([`super::about_content`]).
//!
//! The shell (`window.rs`) owns switching the page in and out, hiding the
//! app chrome and suspending command shortcuts while it is open; this module
//! only builds the widgets and reports which page is on screen.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::contacts_view::ContactFileFormat;
use crate::message_header::{avatar_color_class, initials};
use crate::privacy;

use super::state::{AccountHandle, UiState};
use super::themed_icon_name;

/// The pages, in navigation order: code, label, icon candidates. The first
/// is the one File always opens on, as in Outlook.
const PAGES: &[(&str, &str, &[&str])] = &[
    ("info", "Info", &["emblem-info-symbolic", "dialog-information-symbolic"]),
    ("open", "Open & Export", &["document-open-symbolic", "folder-open-symbolic"]),
    ("options", "Options", &["preferences-system-symbolic", "emblem-system-symbolic"]),
    ("about", "About", &["help-about-symbolic", "dialog-information-symbolic"]),
];

/// The callbacks into the rest of the window. Everything the page needs from
/// the shell arrives here, so the module never reaches into `window.rs` for
/// an action - the same late-bound pattern the composer's "Manage
/// Signatures…" hook uses.
pub(crate) struct Hooks {
    /// Opens the Options page and selects the named Settings category; an
    /// empty name keeps the category already selected. Wired by the shell once
    /// the page's own show/close closures exist.
    pub(crate) open_settings: Rc<dyn Fn(&str)>,
    /// The Settings page itself (`config_view.root`), a File-style category
    /// pane beside the selected category's settings, used as the Options
    /// page. It keeps itself current: every change and every account/calendar
    /// discovery already repaints it through `refresh_config`, so showing it
    /// costs nothing.
    pub(crate) settings_page: gtk::Widget,
    /// The real exit (`app.quit`), behind the rail's pinned row.
    pub(crate) quit: Rc<dyn Fn()>,
    /// Opens the Rules manager, anchored to the tile that was clicked.
    pub(crate) manage_rules: Rc<dyn Fn(&gtk::Widget)>,
    /// Opens the Categories manager, anchored to the tile that was clicked.
    pub(crate) manage_categories: Rc<dyn Fn(&gtk::Widget)>,
    /// Opens the "Import from Evolution…" dialog. The hook is filled in where
    /// the Config row is wired; the flow itself reports when Evolution has
    /// no accounts Lookout is missing.
    pub(crate) import_evolution: Option<super::ImportEvolutionFn>,
    /// Opens the People screen's contact import dialog.
    pub(crate) import_contacts: Rc<dyn Fn()>,
    /// Exports every contact in every address book, in the given format.
    pub(crate) export_contacts: Rc<dyn Fn(ContactFileFormat)>,
}

/// The built page: the root widget plus the handful of inner widgets
/// [`open`](FilePage::open) and [`focus`](FilePage::focus) need later.
pub(crate) struct FilePage {
    /// The whole page, added to the shell's stack as `"file"`.
    pub(crate) root: gtk::Box,
    /// `Back`, wired by the shell to close the page.
    pub(crate) back: gtk::Button,
    /// The page list; selecting a row switches [`stack`](Self::stack).
    nav: gtk::ListBox,
    /// The content pane, one child per page code.
    stack: gtk::Stack,
    /// The Info page's account cards, rebuilt on every open so an account
    /// added in Settings appears without a restart.
    accounts: gtk::Box,
    /// The Info page's "N accounts · N folders · N unread" line.
    summary: gtk::Label,
    /// Kept so an account card's Settings button can open Settings → Accounts.
    open_settings: Rc<dyn Fn(&str)>,
}

impl FilePage {
    /// Shows `code` and refreshes the parts that change while the page is
    /// closed. An unknown code falls back to Info, the default page.
    pub(crate) fn open(&self, state: &UiState, code: &str) {
        let code = PAGES
            .iter()
            .find(|(candidate, _, _)| *candidate == code)
            .map(|(candidate, _, _)| *candidate)
            .unwrap_or("info");
        self.stack.set_visible_child_name(code);
        let Some(index) = PAGES.iter().position(|(candidate, _, _)| *candidate == code) else {
            return;
        };
        let Some(row) = self.nav.row_at_index(index as i32) else {
            if code == "info" {
                self.refresh_info(state);
            }
            return;
        };
        // Selecting a different row fires `row_selected`, which sets the
        // child and refreshes Info; selecting the row that is already
        // selected emits nothing, so refresh here instead. Either way Info is
        // refreshed exactly once. Options needs no repaint of its own: its
        // rows are kept current by `refresh_config` whenever anything changes.
        let already_selected = self.nav.selected_row().is_some_and(|selected| selected == row);
        self.nav.select_row(Some(&row));
        if already_selected && code == "info" {
            self.refresh_info(state);
        }
    }

    /// Puts the keyboard on the page list, where the arrows and Enter work.
    pub(crate) fn focus(&self) {
        if let Some(row) = self.nav.selected_row() {
            row.grab_focus();
        } else {
            self.back.grab_focus();
        }
    }

    /// Rebuilds the Info page's account cards and summary from `state`.
    fn refresh_info(&self, state: &UiState) {
        fill_info(&self.accounts, &self.summary, state, &self.open_settings);
    }
}

/// Repaints the Info page's account list and summary line. The page is built
/// once, so this runs whenever File opens and whenever Info is selected
/// again - an account added in Settings then shows without a restart.
fn fill_info(accounts_box: &gtk::Box, summary: &gtk::Label, state: &UiState, open_settings: &Rc<dyn Fn(&str)>) {
    while let Some(child) = accounts_box.first_child() {
        accounts_box.remove(&child);
    }
    let mut accounts: Vec<&AccountHandle> = state.accounts.values().collect();
    accounts.sort_by_key(|account| {
        let name = if account.display_name.trim().is_empty() {
            &account.email
        } else {
            &account.display_name
        };
        name.to_lowercase()
    });

    if accounts.is_empty() {
        accounts_box.append(&dim_label("No mail accounts are connected yet."));
    } else {
        for account in &accounts {
            accounts_box.append(&account_card(account, open_settings.clone()));
        }
    }

    let folders: usize = accounts.iter().map(|account| account.folders.len()).sum();
    let unread: usize = accounts.iter().flat_map(|account| account.folders.iter()).map(|folder| folder.unread as usize).sum();
    summary.set_label(&format!(
        "{} · {} · {}",
        count(accounts.len(), "account", "accounts"),
        count(folders, "folder", "folders"),
        count(unread, "unread message", "unread messages")
    ));
}

/// Builds the page. The shell adds [`FilePage::root`] to its stack, wires
/// [`FilePage::back`], and calls [`FilePage::open`] whenever File is chosen.
pub(crate) fn build(state: Rc<RefCell<UiState>>, hooks: Hooks) -> FilePage {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["file-page"])
        .hexpand(true)
        .vexpand(true)
        .build();

    // --- The navigation rail -------------------------------------------------
    let rail = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["file-page-nav"]).build();
    rail.set_width_request(210);

    let back_content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();
    let back_icon = gtk::Image::from_icon_name(themed_icon_name(&["go-previous-symbolic", "pan-start-symbolic"]));
    back_icon.set_pixel_size(18);
    let back_label = gtk::Label::builder().label("Back").xalign(0.0).build();
    back_content.append(&back_icon);
    back_content.append(&back_label);
    let back = gtk::Button::builder()
        .child(&back_content)
        .css_classes(["flat", "file-nav-back"])
        .halign(gtk::Align::Fill)
        .build();
    back.set_tooltip_text(Some("Back to the mailbox"));
    back.update_property(&[gtk::accessible::Property::Label("Back")]);
    rail.append(&back);

    let nav = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["file-page-nav-list"])
        .vexpand(true)
        .build();
    nav.set_margin_top(10);
    for (_code, label, icons) in PAGES {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .margin_start(8)
            .margin_end(8)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        // The 3 px accent bar that marks the page in view in the neutral
        // (dark) rail; transparent on the accent-filled light rail.
        let marker = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).build();
        marker.set_width_request(3);
        marker.set_height_request(18);
        marker.add_css_class("file-nav-marker");
        let icon = gtk::Image::from_icon_name(themed_icon_name(icons));
        icon.set_pixel_size(16);
        let text = gtk::Label::builder().label(*label).xalign(0.0).build();
        row_box.append(&marker);
        row_box.append(&icon);
        row_box.append(&text);
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        row.set_selectable(true);
        row.set_child(Some(&row_box));
        row.update_property(&[gtk::accessible::Property::Label(label)]);
        nav.append(&row);
    }
    rail.append(&nav);

    // Quit stays reachable now that the old File menu is gone. It is the real
    // exit (`app.quit`), not the window close button's close-to-background.
    let quit_content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();
    let quit_icon = gtk::Image::from_icon_name(themed_icon_name(&["application-exit-symbolic", "window-close-symbolic"]));
    quit_icon.set_pixel_size(16);
    let quit_label = gtk::Label::builder().label("Quit Lookout").xalign(0.0).build();
    quit_content.append(&quit_icon);
    quit_content.append(&quit_label);
    let quit = gtk::Button::builder()
        .child(&quit_content)
        .css_classes(["flat", "file-nav-quit"])
        .halign(gtk::Align::Fill)
        .build();
    quit.set_tooltip_text(Some("Quit Lookout completely, stopping mail and calendar sync"));
    quit.update_property(&[gtk::accessible::Property::Label("Quit Lookout")]);
    {
        let quit_hook = hooks.quit.clone();
        quit.connect_clicked(move |_| quit_hook());
    }
    // `nav` takes all the rail's spare height, so appending Quit after it
    // pins the row to the foot without a separate spacer.
    rail.append(&quit);
    rail.set_vexpand(true);
    root.append(&rail);

    // --- The content pane ----------------------------------------------------
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::None)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["file-page-content"])
        .build();
    // Each page is measured only while it is the visible one: the Options
    // page's settings column is large, and homogeneous measurement (the
    // default) would measure it on every Info/About allocation too.
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    let info_body = page_body();
    info_body.append(&heading("Info"));
    info_body.append(&section("Accounts"));
    let accounts = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    info_body.append(&accounts);
    {
        let open_settings = hooks.open_settings.clone();
        let add = tile("list-add-symbolic", "Add Account", "Connect a GNOME Online Account or an IMAP/SMTP mailbox", move |_| {
            open_settings("accounts")
        });
        info_body.append(&add);
    }
    info_body.append(&section("Mailbox"));
    let summary = dim_label("");
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    info_body.append(&summary);
    info_body.append(&section("Rules and categories"));
    {
        let manage = hooks.manage_rules.clone();
        info_body.append(&tile("emblem-system-symbolic", "Manage Rules", "What happens to mail as it arrives", move |button| {
            manage(button.upcast_ref())
        }));
    }
    {
        let manage = hooks.manage_categories.clone();
        info_body.append(&tile(
            "view-list-symbolic",
            "Categories",
            "Names and colours for sorting mail, events, people and tasks",
            move |button| manage(button.upcast_ref()),
        ));
    }
    add_page(&stack, "info", &info_body);

    let open_body = page_body();
    open_body.append(&heading("Open & Export"));
    open_body.append(&section("Bring in"));
    if let Some(import) = hooks.import_evolution.clone() {
        open_body.append(&tile(
            "document-open-symbolic",
            "Import from Evolution…",
            "Add the IMAP/SMTP accounts already set up in Evolution",
            move |button| import(button.upcast_ref()),
        ));
    }
    {
        let import = hooks.import_contacts.clone();
        open_body.append(&tile(
            "contact-new-symbolic",
            "Import Contacts…",
            "Add contacts to an address book from vCard (.vcf) or Outlook CSV files",
            move |_| import(),
        ));
    }
    open_body.append(&section("Save out"));
    {
        let export = hooks.export_contacts.clone();
        open_body.append(&tile(
            "document-save-symbolic",
            "Export Contacts as vCard…",
            "Every contact, in every address book, as one .vcf file",
            move |_| export(ContactFileFormat::VCard),
        ));
    }
    {
        let export = hooks.export_contacts.clone();
        open_body.append(&tile(
            "document-save-symbolic",
            "Export Contacts as Outlook CSV…",
            "Every contact, in every address book, in Outlook's CSV format",
            move |_| export(ContactFileFormat::OutlookCsv),
        ));
    }
    add_page(&stack, "open", &open_body);

    // Options *is* the Settings page, rebuilt in the File style: a category
    // pane on the left and the selected category's settings on the right. The
    // view owns its two panes, scrolling and margins, so it is added directly.
    let settings_page = hooks.settings_page.clone();
    settings_page.set_hexpand(true);
    settings_page.set_vexpand(true);
    stack.add_named(&settings_page, Some("options"));

    let about_body = page_body();
    about_body.append(&super::about_content());
    add_page(&stack, "about", &about_body);

    root.append(&stack);

    {
        let stack = stack.clone();
        let accounts_box = accounts.clone();
        let summary_label = summary.clone();
        let open_settings = hooks.open_settings.clone();
        let state = state.clone();
        nav.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let Some((code, _, _)) = PAGES.get(row.index() as usize) else { return };
            stack.set_visible_child_name(code);
            // Coming back to Info after changing accounts picks up the new
            // list; `open` has already done this on first show. Options is
            // always current (see `Hooks::settings_page`).
            if *code == "info" {
                fill_info(&accounts_box, &summary_label, &state.borrow(), &open_settings);
            }
        });
    }

    let page = FilePage {
        root,
        back,
        nav,
        stack,
        accounts,
        summary,
        open_settings: hooks.open_settings.clone(),
    };
    page.refresh_info(&state.borrow());
    page
}

/// A centred, scrollable content column: 36 px side margins, 24 px top and
/// bottom, at most 760 px wide, as the MailCraft Backstage lays its pages
/// out. Returns the page's body - append the heading, sections and tiles to
/// it - and the caller wraps it with [`add_page`].
fn page_body() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(36)
        .margin_end(36)
        .build()
}

/// Puts `body` in the scrollable, width-clamped content column and adds it to
/// the page stack under `code`.
fn add_page(stack: &gtk::Stack, code: &str, body: &gtk::Box) {
    let clamp = adw::Clamp::builder().maximum_size(760).child(body).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&clamp)
        .build();
    stack.add_named(&scroller, Some(code));
}

/// A page heading, 22 px semibold (the size is in `file-page.css`).
fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["file-page-heading"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .build()
}

/// A section label above a run of tiles: semibold, in the accent colour.
fn section(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["file-page-section"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .margin_top(10)
        .build()
}

/// A dim, wrapping line of explanatory text.
fn dim_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["dim-label"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build()
}

/// A wide button with an icon, a title and a line of explanation - the tile
/// shape both the File pages and Outlook's Backstage use. The callback gets
/// the button itself, so a flow that opens a dialog can anchor it there.
fn tile<F>(icon: &'static str, title: &str, detail: &str, on_click: F) -> gtk::Button
where
    F: Fn(&gtk::Button) + 'static,
{
    let image = gtk::Image::from_icon_name(themed_icon_name(&[icon, "image-missing-symbolic"]));
    image.set_pixel_size(24);
    image.set_valign(gtk::Align::Center);
    let title_label = gtk::Label::builder()
        .label(title)
        .css_classes(["file-page-tile-title"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let detail_label = gtk::Label::builder()
        .label(detail)
        .css_classes(["file-page-tile-detail"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(&title_label);
    text.append(&detail_label);
    let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(14).build();
    content.set_margin_start(14);
    content.set_margin_end(14);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.append(&image);
    content.append(&text);
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["flat", "file-page-tile"])
        .halign(gtk::Align::Fill)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&format!("{title}. {detail}"))]);
    button.connect_clicked(move |button| on_click(button));
    button
}

/// One account on the Info page: initials, name and address, the servers it
/// talks to, and a Settings button that opens Settings → Accounts.
fn account_card(account: &AccountHandle, open_settings: Rc<dyn Fn(&str)>) -> gtk::Box {
    let (name, address) = if account.display_name.trim().is_empty() {
        (account.email.clone(), account.email.clone())
    } else {
        (account.display_name.clone(), account.email.clone())
    };

    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .css_classes(["file-page-card"])
        .build();

    let avatar = gtk::Label::builder()
        .label(initials(Some(&name), &address))
        .css_classes(["avatar-circle"])
        .valign(gtk::Align::Center)
        .build();
    avatar.set_width_request(36);
    avatar.set_height_request(36);
    avatar.add_css_class(avatar_color_class(&address));
    // Initials and address identify the account, so privacy mode blurs them.
    privacy::mark(&avatar);
    card.append(&avatar);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let name_label = gtk::Label::builder()
        .label(name)
        .css_classes(["file-page-card-title"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    privacy::mark(&name_label);
    let address_label = gtk::Label::builder()
        .label(address)
        .css_classes(["file-page-card-detail"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    privacy::mark(&address_label);
    let servers = if account.imap_host.trim().is_empty() {
        "Mail account".to_string()
    } else {
        format!("IMAP {} · SMTP {}", account.imap_host, account.smtp_host)
    };
    let server_label = gtk::Label::builder()
        .label(servers)
        .css_classes(["file-page-card-detail"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    text.append(&name_label);
    text.append(&address_label);
    text.append(&server_label);
    card.append(&text);

    let settings = gtk::Button::with_label("Settings");
    settings.set_css_classes(&["flat", "file-page-card-button"]);
    settings.set_valign(gtk::Align::Center);
    settings.set_tooltip_text(Some("Open this account's settings"));
    {
        let open_settings = open_settings.clone();
        settings.connect_clicked(move |_| open_settings("accounts"));
    }
    card.append(&settings);

    card
}

/// `n` with the right English word, for the Info page's summary line.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}
