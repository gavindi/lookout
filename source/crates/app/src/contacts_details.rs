//! The People screen's details pane (UX-65): the right-hand column that shows
//! one contact at a time, next to the middle list. It mirrors Outlook's People
//! layout - photo, name and title/organization, an action row, then the
//! **Overview / Contact info / Files / Emails** tabs - and is where a contact
//! is now edited in place (the dialog survives for create and pop-out, see
//! `contacts_editor.rs`).
//!
//! Rebuilt-widget-out rather than reactive: `ContactsDetailsPane` holds the
//! tab hosts, and `set_contact` clears and refills them for the selected
//! entry, so a selection change or a sync repaint is one call. The pane is
//! data-in/callback-out like the profile card - the host window supplies the
//! [`ContactsDetailsActions`] closures - and it reuses the profile card's
//! upcoming-events shaping (`shared_upcoming_events`).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{EmailAddress, EmailSummary, VCard};
use lookout_graph::DirectoryPerson;

use crate::contacts_view::{contact_photo_texture, directory_person_display_name, directory_person_secondary, vcard_display_name, vcard_primary_email, ContactsListEntry};
use crate::profile_card::{EventsForPersonFn, OpenMessageFn, PersonActionFn, SendEmailFn};
use crate::util::{action_button, clear_box};
use crate::window::UiState;
use crate::worker::Worker;

/// How many recent messages the Emails tab lists.
const RECENT_MAIL_LIMIT: usize = 10;
/// How many cached messages the Files tab scans for attachments.
const FILES_SCAN_LIMIT: usize = 100;
/// How many attachments the Files tab lists.
const FILES_LIMIT: usize = 40;
/// How far ahead the Overview tab's events section looks.
const EVENT_HORIZON_DAYS: i64 = 90;
const UPCOMING_EVENTS_LIMIT: usize = 3;

/// The actions the details pane offers, built by the host window (which owns
/// the composer, the event editor and the contact dialogs). Reuses the
/// profile card's function types so both surfaces share one wiring.
#[derive(Clone)]
pub struct ContactsDetailsActions {
    pub send_email: SendEmailFn,
    pub schedule_meeting: PersonActionFn,
    pub open_message: OpenMessageFn,
    pub events_for_person: EventsForPersonFn,
    /// Open the contact editor in the pane (the host handles the dialog
    /// fallback when the entry isn't writable, and opens GNOME Contacts for a
    /// desktop contact).
    pub edit: Rc<dyn Fn(&ContactsListEntry)>,
    /// Delete the contact (with the host's usual Undo toast).
    pub delete: Rc<dyn Fn(&ContactsListEntry)>,
    /// Open an external URI (an IMPP/URL from the card) in the default handler.
    pub open_uri: Rc<dyn Fn(&str)>,
    /// Save a live company-directory hit as a contact (item 4.7.8): opens the
    /// create form seeded with the directory's name, address, title and
    /// company.
    pub add_directory_contact: Rc<dyn Fn(&DirectoryPerson)>,
}

/// The details pane's persistent widgets. `root` swaps between the empty state
/// and the details column; the `adw::ViewStack` holds the four tabs, each a
/// scrolled host box that `set_contact` refills.
#[derive(Clone)]
pub struct ContactsDetailsPane {
    pub root: gtk::Stack,
    /// The contact header (photo/name/title), refilled per selection.
    pub header_slot: gtk::Box,
    /// The action row (Send email / Schedule / Chat / Edit / Delete).
    pub actions_slot: gtk::Box,
    pub overview: gtk::Box,
    pub contact_info: gtk::Box,
    pub emails: gtk::Box,
    pub files: gtk::Box,
    /// The inline editor's host (UX-65), shown as the `edit` page.
    pub edit_slot: gtk::Box,
    /// The Overview / Contact info / Files / Emails switcher and stack, kept
    /// so directory mode (item 4.7.8) can hide the tabs and pin Overview.
    switcher: adw::ViewSwitcher,
    view_stack: adw::ViewStack,
    /// Bumped on every `set_contact`, so an in-flight Emails/Files load from a
    /// previous selection doesn't append into a now-reused tab.
    generation: Rc<Cell<u64>>,
}

impl ContactsDetailsPane {
    /// Builds the pane with its empty state showing.
    pub fn build() -> Self {
        let root = gtk::Stack::builder().vexpand(true).hexpand(true).build();

        let empty = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .margin_start(18)
            .margin_end(18)
            .build();
        empty.append(&gtk::Image::from_icon_name("avatar-default-symbolic").upcast::<gtk::Widget>());
        let empty_note = gtk::Label::builder()
            .label("Select a contact to see details")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build();
        crate::privacy::mark_ui(&empty_note);
        empty.append(&empty_note);
        root.add_named(&empty, Some("empty"));

        let details = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        let header_slot = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let actions_slot = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
        actions_slot.set_halign(gtk::Align::Start);

        let view_stack = adw::ViewStack::new();
        let overview = tab_box();
        let contact_info = tab_box();
        let emails = tab_box();
        let files = tab_box();
        // The switcher is narrow, so libadwaita stacks each icon above its
        // label; all four names exist in both the GNOME theme and the bundled
        // Fluent pack, so the tabs keep their icons in either icon style.
        view_stack.add_titled_with_icon(&scrolled(&overview), Some("overview"), "Overview", "avatar-default-symbolic");
        view_stack.add_titled_with_icon(&scrolled(&contact_info), Some("contact-info"), "Contact info", "user-info-symbolic");
        view_stack.add_titled_with_icon(&scrolled(&files), Some("files"), "Files", "mail-attachment-symbolic");
        view_stack.add_titled_with_icon(&scrolled(&emails), Some("emails"), "Emails", "mail-unread-symbolic");
        let switcher = adw::ViewSwitcher::builder().policy(adw::ViewSwitcherPolicy::Narrow).build();
        switcher.set_stack(Some(&view_stack));

        details.append(&header_slot);
        details.append(&actions_slot);
        details.append(&switcher);
        details.append(&view_stack);
        root.add_named(&details, Some("details"));

        // The inline editor page (UX-65.7): a scrolled host the window fills
        // with a contact form when Edit is chosen.
        let edit_slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        root.add_named(&scrolled(&edit_slot), Some("edit"));
        root.set_visible_child_name("empty");

        Self {
            root,
            header_slot,
            actions_slot,
            overview,
            contact_info,
            emails,
            files,
            edit_slot,
            switcher,
            view_stack,
            generation: Rc::new(Cell::new(0)),
        }
    }

    /// Repaints the pane for `entry`, or shows the empty state when it is
    /// `None`. Synchronous pieces (header, actions, Overview, Contact info)
    /// are built now; the Emails and Files tabs load from the caches
    /// asynchronously and fill in when they answer.
    pub fn set_contact(&self, entry: Option<&ContactsListEntry>, state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, actions: &ContactsDetailsActions) {
        clear_box(&self.header_slot);
        clear_box(&self.actions_slot);
        for tab in [&self.overview, &self.contact_info, &self.emails, &self.files] {
            clear_box(tab);
        }

        let Some(entry) = entry else {
            self.root.set_visible_child_name("empty");
            return;
        };
        self.root.set_visible_child_name("details");
        self.switcher.set_visible(true);

        let display_name = vcard_display_name(entry.card());
        let primary_email = vcard_primary_email(entry.card());
        let address = EmailAddress {
            name: Some(display_name.clone()),
            address: primary_email.clone(),
        };

        self.header_slot.append(&build_header(entry.card(), &display_name, &primary_email));
        self.actions_slot.append(&build_actions(entry, &address, actions));

        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        fill_overview(&self.overview, entry, actions);
        fill_contact_info(&self.contact_info, entry.card());
        spawn_recent_mail(&self.emails, &address, entry, state, worker, actions, &self.generation, generation);
        spawn_files(&self.files, &address, entry, state, worker, actions, &self.generation, generation);
    }

    /// Repaints the pane for a live company-directory hit (item 4.7.8). The
    /// pane's four stored-contact tabs don't apply to someone who isn't in the
    /// address book, so the switcher is hidden and Overview carries the
    /// directory's fields plus Send email / Add to contacts.
    pub fn set_directory_person(&self, person: Option<&DirectoryPerson>, actions: &ContactsDetailsActions) {
        clear_box(&self.header_slot);
        clear_box(&self.actions_slot);
        for tab in [&self.overview, &self.contact_info, &self.emails, &self.files] {
            clear_box(tab);
        }
        // Cancel any Emails/Files load still in flight for the last contact.
        self.generation.set(self.generation.get().wrapping_add(1));

        let Some(person) = person else {
            self.root.set_visible_child_name("empty");
            return;
        };
        self.root.set_visible_child_name("details");
        self.switcher.set_visible(false);
        self.view_stack.set_visible_child_name("overview");

        self.header_slot.append(&build_directory_header(person));
        self.actions_slot.append(&build_directory_actions(person, actions));
        fill_directory_overview(&self.overview, person);
    }

    /// Whether the pane is currently showing the inline editor (UX-65.7).
    /// Callers skip a repaint while so, or a background sync would discard the
    /// form the user is filling in.
    pub fn is_editing(&self) -> bool {
        self.root.visible_child_name().as_deref() == Some("edit")
    }

    /// Returns the pane to the details view (after Save, Delete or Cancel).
    pub fn show_details(&self) {
        self.root.set_visible_child_name("details");
    }

    /// Opens the inline editor for `entry`: a contact form prefilled from the
    /// card, with Save / Cancel / Delete. The host supplies the callbacks
    /// (`on_save` gets the built card, `on_delete` the request, `on_cancel`
    /// the dismissal) so this module never routes to a session itself.
    pub fn begin_edit<F, G, H>(&self, entry: &ContactsListEntry, on_save: F, on_delete: G, on_cancel: H)
    where
        F: Fn(lookout_core::VCard) + 'static,
        G: Fn() + 'static,
        H: Fn() + 'static,
    {
        clear_box(&self.edit_slot);

        let form = crate::contacts_editor::build_contact_form(Some(entry.card()), None, true);
        self.edit_slot.append(&form.body);

        let footer = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        footer.set_halign(gtk::Align::End);
        let delete_button = gtk::Button::with_label("Delete");
        delete_button.add_css_class("destructive-action");
        delete_button.set_sensitive(!entry.href().is_empty());
        let cancel_button = gtk::Button::with_label("Cancel");
        let save_button = gtk::Button::with_label("Save");
        save_button.add_css_class("suggested-action");
        footer.append(&delete_button);
        footer.append(&cancel_button);
        footer.append(&save_button);
        self.edit_slot.append(&footer);

        {
            let on_cancel = Rc::new(on_cancel);
            cancel_button.connect_clicked(move |_| on_cancel());
        }
        {
            let build_card = form.build_card.clone();
            let on_save = Rc::new(on_save);
            save_button.connect_clicked(move |_| {
                if let Some(card) = build_card() {
                    on_save(card);
                }
            });
        }
        {
            let on_delete = Rc::new(on_delete);
            delete_button.connect_clicked(move |_| on_delete());
        }

        self.root.set_visible_child_name("edit");
    }
}

/// A tab's content host: a vertical box with a little breathing room inside
/// the scrolled wrapper.
fn tab_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build()
}

fn scrolled(content: &gtk::Box) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build()
}

/// The contact header: avatar/photo, name, title · organization and primary
/// email.
fn build_header(card: &VCard, display_name: &str, primary_email: &str) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();

    let avatar_label = gtk::Label::builder()
        .css_classes(["avatar-circle"])
        .label(crate::message_header::initials(Some(display_name), primary_email))
        .build();
    avatar_label.add_css_class(crate::message_header::avatar_color_class(primary_email));
    let avatar_photo = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).css_classes(["avatar-photo"]).build();
    if let Some(texture) = card.photo.as_ref().and_then(contact_photo_texture) {
        avatar_photo.set_paintable(Some(&texture));
    }
    let avatar = gtk::Overlay::builder()
        .width_request(64)
        .height_request(64)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .child(&avatar_label)
        .build();
    avatar.add_overlay(&avatar_photo);
    crate::privacy::mark(&avatar);
    row.append(&avatar);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(&gtk::Label::builder().label(display_name).xalign(0.0).wrap(true).css_classes(["heading"]).build());
    if let Some(secondary) = organization_line(card) {
        text.append(&gtk::Label::builder().label(&secondary).xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build());
    }
    if !primary_email.trim().is_empty() {
        text.append(&gtk::Label::builder().label(primary_email).xalign(0.0).wrap(true).css_classes(["caption"]).build());
    }
    crate::privacy::mark(&text);
    row.append(&text);
    row.upcast()
}

/// The "Title · Organization" line, either part omitted when absent.
fn organization_line(card: &VCard) -> Option<String> {
    let org = card
        .organization
        .as_ref()
        .map(|parts| parts.iter().map(|part| part.trim()).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" / "))
        .filter(|value| !value.is_empty());
    let title = card.title.as_deref().map(str::trim).filter(|value| !value.is_empty());
    match (title, org) {
        (Some(title), Some(org)) => Some(format!("{title} · {org}")),
        (Some(title), None) => Some(title.to_string()),
        (None, Some(org)) => Some(org),
        (None, None) => None,
    }
}

/// The action row: Send email · Schedule meeting · Chat/Website · Edit ·
/// Delete.
fn build_actions(entry: &ContactsListEntry, address: &EmailAddress, actions: &ContactsDetailsActions) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();

    let send = action_button("mail-send-symbolic", "Send email");
    send.set_tooltip_text(Some("Send a new message to this contact"));
    {
        let actions = actions.clone();
        let address = address.clone();
        let account_id = entry.account_id.clone();
        send.connect_clicked(move |_| (actions.send_email)(&address, Some(account_id.clone())));
    }
    row.append(&send);

    let schedule = action_button("x-office-calendar-symbolic", "Schedule meeting");
    schedule.set_tooltip_text(Some("Schedule a meeting with this contact"));
    {
        let actions = actions.clone();
        let address = address.clone();
        schedule.connect_clicked(move |_| (actions.schedule_meeting)(&address));
    }
    row.append(&schedule);

    // GNOME has no telephony, so where Outlook offers "Call" Lookout offers
    // the contact's own chat address (IMPP) or website instead.
    let (icon, label, uri) = if let Some(uri) = entry.card().impp.iter().map(|im| im.uri.trim()).find(|uri| !uri.is_empty()) {
        ("user-available-symbolic", "Chat", Some(uri.to_string()))
    } else if let Some(uri) = entry.card().urls.iter().map(|url| url.trim()).find(|url| !url.is_empty()) {
        ("web-browser-symbolic", "Website", Some(uri.to_string()))
    } else {
        ("user-available-symbolic", "Chat", None)
    };
    let chat = action_button(icon, label);
    match uri {
        Some(uri) => {
            chat.set_tooltip_text(Some(&uri));
            let actions = actions.clone();
            chat.connect_clicked(move |_| (actions.open_uri)(&uri));
        }
        None => {
            chat.set_sensitive(false);
            chat.set_tooltip_text(Some("No chat address or website for this contact"));
        }
    }
    row.append(&chat);

    // A desktop contact (E1.5) is read-only here: its "edit" opens GNOME
    // Contacts instead, through the same `edit` action.
    let edit = if crate::desktop_contacts::is_desktop_entry(entry) {
        let button = action_button("x-office-address-book-symbolic", "Open in GNOME Contacts");
        button.set_tooltip_text(Some("Desktop contacts are read-only in Lookout - edit them in GNOME Contacts"));
        button
    } else {
        let button = action_button("document-edit-symbolic", "Edit");
        button.set_tooltip_text(Some("Edit this contact in the details pane"));
        button
    };
    {
        let actions = actions.clone();
        let entry = entry.clone();
        edit.connect_clicked(move |_| (actions.edit)(&entry));
    }
    row.append(&edit);

    let delete = action_button("user-trash-symbolic", "Delete");
    delete.set_tooltip_text(Some("Delete this contact"));
    delete.set_sensitive(!entry.href().is_empty());
    {
        let actions = actions.clone();
        let entry = entry.clone();
        delete.connect_clicked(move |_| (actions.delete)(&entry));
    }
    row.append(&delete);

    row.upcast()
}

/// The directory hit's header (item 4.7.8): avatar initials, name, the
/// "Title · Company" line the directory gave us, and the address when it
/// isn't already the secondary line.
fn build_directory_header(person: &DirectoryPerson) -> gtk::Widget {
    let display_name = directory_person_display_name(person);
    let secondary = directory_person_secondary(person);
    let address = person.address.trim();

    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();

    let avatar_label = gtk::Label::builder()
        .css_classes(["avatar-circle"])
        .label(crate::message_header::initials(Some(&display_name), address))
        .build();
    avatar_label.add_css_class(crate::message_header::avatar_color_class(address));
    let avatar = gtk::Overlay::builder()
        .width_request(64)
        .height_request(64)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .child(&avatar_label)
        .build();
    crate::privacy::mark(&avatar);
    row.append(&avatar);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(&gtk::Label::builder().label(&display_name).xalign(0.0).wrap(true).css_classes(["heading"]).build());
    if !secondary.is_empty() {
        text.append(&gtk::Label::builder().label(&secondary).xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build());
    }
    if !address.is_empty() && address != secondary {
        text.append(&gtk::Label::builder().label(address).xalign(0.0).wrap(true).css_classes(["caption"]).build());
    }
    crate::privacy::mark(&text);
    row.append(&text);
    row.upcast()
}

/// The directory hit's action row (item 4.7.8): Send email and Add to
/// contacts. Editing/deleting doesn't apply - the person isn't in the address
/// book yet.
fn build_directory_actions(person: &DirectoryPerson, actions: &ContactsDetailsActions) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
    let address = EmailAddress {
        name: person.name.clone().filter(|name| !name.trim().is_empty()),
        address: person.address.trim().to_string(),
    };

    let send = action_button("mail-send-symbolic", "Send email");
    send.set_tooltip_text(Some("Send a new message to this person"));
    send.set_sensitive(!address.address.is_empty());
    {
        let actions = actions.clone();
        let address = address.clone();
        send.connect_clicked(move |_| (actions.send_email)(&address, None));
    }
    row.append(&send);

    let add = action_button("contact-new-symbolic", "Add to contacts");
    add.set_tooltip_text(Some("Save this person in one of your address books"));
    {
        let actions = actions.clone();
        let person = person.clone();
        add.connect_clicked(move |_| (actions.add_directory_contact)(&person));
    }
    row.append(&add);

    row.upcast()
}

/// Fills the Overview tab for a directory hit: the fields the directory
/// carries, with a note that this person isn't in the user's contacts yet.
fn fill_directory_overview(container: &gtk::Box, person: &DirectoryPerson) {
    container.append(&section_heading("Company directory"));

    let address = person.address.trim();
    if !address.is_empty() {
        container.append(&section_heading("Email"));
        container.append(&info_value(address));
    }
    if let Some(title) = person.title.as_deref().map(str::trim).filter(|title| !title.is_empty()) {
        container.append(&section_heading("Title"));
        container.append(&info_value(title));
    }
    if let Some(company) = person.company.as_deref().map(str::trim).filter(|company| !company.is_empty()) {
        container.append(&section_heading("Company"));
        container.append(&info_value(company));
    }
}

/// Fills the Overview tab: the key contact fields, then upcoming shared
/// events.
fn fill_overview(container: &gtk::Box, entry: &ContactsListEntry, actions: &ContactsDetailsActions) {
    let card = entry.card();
    let mut any = false;
    if !card.emails.is_empty() {
        container.append(&section_heading("Email"));
        for email in &card.emails {
            container.append(&info_value(&email.address));
        }
        any = true;
    }
    if !card.telephones.is_empty() {
        container.append(&section_heading("Phone"));
        for phone in &card.telephones {
            container.append(&info_value(&phone.number));
        }
        any = true;
    }
    if !card.addresses.is_empty() {
        container.append(&section_heading("Address"));
        for address in &card.addresses {
            let line = format_address(address);
            if !line.is_empty() {
                container.append(&info_value(&line));
            }
        }
        any = true;
    }
    if let Some(birthday) = card.birthday {
        container.append(&section_heading("Birthday"));
        container.append(&info_value(&format_contact_date(birthday)));
        any = true;
    }
    if !any {
        container.append(&empty_label("No contact details yet"));
    }

    // Upcoming shared events (already in memory, so synchronous).
    let address = card.emails.first().map(|email| email.address.clone()).unwrap_or_default();
    let now = chrono::Local::now().naive_local();
    let occurrences = (actions.events_for_person)(&address);
    let events = crate::profile_card::shared_upcoming_events(&occurrences, &address, now, chrono::Duration::days(EVENT_HORIZON_DAYS), UPCOMING_EVENTS_LIMIT);
    if !events.is_empty() {
        container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        container.append(&section_heading("Upcoming events"));
        for occ in &events {
            let (start, _end) = crate::calendar_view::local_span(occ);
            let time = if occ.all_day {
                start.format("%a %d %b").to_string()
            } else {
                start.format("%a %d %b %H:%M").to_string()
            };
            let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).build();
            text.append(
                &gtk::Label::builder()
                    .label(occ.summary.as_deref().unwrap_or("(no subject)"))
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            );
            text.append(&gtk::Label::builder().label(&time).xalign(0.0).css_classes(["caption", "dim-label"]).build());
            container.append(&text);
        }
    }
}

/// Fills the Contact info tab: every field the card carries, grouped.
fn fill_contact_info(container: &gtk::Box, card: &VCard) {
    let mut any = false;
    let block = |container: &gtk::Box, heading: &str, values: Vec<String>, any: &mut bool| {
        let values: Vec<String> = values.into_iter().filter(|value| !value.trim().is_empty()).collect();
        if values.is_empty() {
            return;
        }
        container.append(&section_heading(heading));
        for value in values {
            container.append(&info_value(&value));
        }
        *any = true;
    };

    block(container, "Emails", card.emails.iter().map(|email| email.address.clone()).collect(), &mut any);
    block(container, "Phones", card.telephones.iter().map(|phone| phone.number.clone()).collect(), &mut any);
    block(container, "Addresses", card.addresses.iter().map(format_address).collect(), &mut any);
    block(container, "Websites", card.urls.clone(), &mut any);
    block(container, "Instant messaging", card.impp.iter().map(|im| im.uri.clone()).collect(), &mut any);
    if let Some(birthday) = card.birthday {
        block(container, "Birthday", vec![format_contact_date(birthday)], &mut any);
    }
    if let Some(anniversary) = card.anniversary {
        block(container, "Anniversary", vec![format_contact_date(anniversary)], &mut any);
    }
    if let Some(org) = &card.organization {
        let value = org.iter().map(|part| part.trim()).filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" / ");
        block(container, "Organization", vec![value], &mut any);
    }
    if let Some(title) = &card.title {
        block(container, "Title", vec![title.trim().to_string()], &mut any);
    }
    block(
        container,
        "Custom fields",
        card.other.iter().map(|property| format!("{}: {}", property.name, property.value.trim())).collect(),
        &mut any,
    );
    if let Some(note) = &card.note {
        block(container, "Notes", vec![note.trim().to_string()], &mut any);
    }
    if !any {
        container.append(&empty_label("No contact details yet"));
    }
}

/// Loads the contact's most recent cached messages and fills the Emails tab.
#[allow(clippy::too_many_arguments)]
fn spawn_recent_mail(
    container: &gtk::Box,
    address: &EmailAddress,
    entry: &ContactsListEntry,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    actions: &ContactsDetailsActions,
    generation: &Rc<Cell<u64>>,
    expected: u64,
) {
    let container = container.clone();
    let generation = generation.clone();
    let query = lookout_mail::SearchQuery {
        from: vec![address.address.clone()],
        ..Default::default()
    };
    let caches = account_caches(state, &entry.account_id);
    if caches.is_empty() {
        container.append(&empty_label("No recent messages"));
        return;
    }
    let receivers: Vec<async_channel::Receiver<Vec<EmailSummary>>> = caches
        .into_iter()
        .map(|cache| {
            let query = query.clone();
            crate::window::spawn_cache_read(worker, cache, move |cache| cache.search_query(&query, None, RECENT_MAIL_LIMIT).unwrap_or_default())
        })
        .collect();

    let actions = actions.clone();
    glib::spawn_future_local(async move {
        let mut messages: Vec<EmailSummary> = Vec::new();
        for receiver in receivers {
            if let Ok(found) = receiver.recv().await {
                messages.extend(found);
            }
        }
        messages.sort_by_key(|message| std::cmp::Reverse(message.date));
        messages.truncate(RECENT_MAIL_LIMIT);
        // A newer selection has taken over these boxes; drop this result.
        if generation.get() != expected {
            return;
        }
        if messages.is_empty() {
            container.append(&empty_label("No recent messages"));
            return;
        }
        for message in &messages {
            container.append(&mail_row(&actions, message));
        }
    });
}

/// One recent-mail row: subject with the date on the right; clicking opens the
/// message.
fn mail_row(actions: &ContactsDetailsActions, message: &EmailSummary) -> gtk::Widget {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(
        &gtk::Label::builder()
            .label(message.subject.as_deref().unwrap_or("(no subject)"))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    row.append(
        &gtk::Label::builder()
            .label(message.date.with_timezone(&chrono::Local).format("%d/%m/%Y").to_string())
            .css_classes(["caption", "dim-label"])
            .valign(gtk::Align::Center)
            .build(),
    );
    let button = gtk::Button::builder().child(&row).css_classes(["flat", "profile-card-row"]).build();
    // Subject/date are content: blurred by privacy mode.
    crate::privacy::mark_content(&button);
    let actions = actions.clone();
    let message = message.clone();
    button.connect_clicked(move |_| (actions.open_message)(&message));
    button.upcast()
}

/// One cached attachment a contact has sent or received.
struct PersonFile {
    name: String,
    size: u32,
    content_type: String,
    date: chrono::DateTime<chrono::Utc>,
    message: EmailSummary,
}

/// Scans the contact's cached messages for attachments and fills the Files
/// tab. Attachment names come from each message's `BODYSTRUCTURE` (already in
/// the cache with the envelope), so no body fetch is needed.
#[allow(clippy::too_many_arguments)]
fn spawn_files(
    container: &gtk::Box,
    address: &EmailAddress,
    entry: &ContactsListEntry,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    actions: &ContactsDetailsActions,
    generation: &Rc<Cell<u64>>,
    expected: u64,
) {
    let container = container.clone();
    let generation = generation.clone();
    let query = lookout_mail::SearchQuery {
        from: vec![address.address.clone()],
        has_attachment: true,
        ..Default::default()
    };
    let caches = account_caches(state, &entry.account_id);
    if caches.is_empty() {
        container.append(&empty_label("No files from this contact yet"));
        return;
    }
    let receivers: Vec<async_channel::Receiver<Vec<PersonFile>>> = caches
        .into_iter()
        .map(|cache| {
            let query = query.clone();
            crate::window::spawn_cache_read(worker, cache, move |cache| {
                let summaries = cache.search_query(&query, None, FILES_SCAN_LIMIT).unwrap_or_default();
                summaries
                    .into_iter()
                    .flat_map(|summary| {
                        let parts: Vec<(String, u32, String)> = summary
                            .structure
                            .iter()
                            .flatten()
                            .filter(|part| part.is_attachment)
                            .map(|part| (part.filename.clone().unwrap_or_else(|| "attachment".to_string()), part.size, part.content_type.clone()))
                            .collect();
                        parts.into_iter().map(move |(name, size, content_type)| PersonFile {
                            name,
                            size,
                            content_type,
                            date: summary.date,
                            message: summary.clone(),
                        })
                    })
                    .collect()
            })
        })
        .collect();

    let actions = actions.clone();
    glib::spawn_future_local(async move {
        let mut files: Vec<PersonFile> = Vec::new();
        for receiver in receivers {
            if let Ok(found) = receiver.recv().await {
                files.extend(found);
            }
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.date));
        files.truncate(FILES_LIMIT);
        // A newer selection has taken over these boxes; drop this result.
        if generation.get() != expected {
            return;
        }
        if files.is_empty() {
            container.append(&empty_label("No files from this contact yet"));
            return;
        }
        for file in &files {
            container.append(&file_row(&actions, file));
        }
    });
}

/// One file row: a type icon, the name, and its size and date.
fn file_row(actions: &ContactsDetailsActions, file: &PersonFile) -> gtk::Widget {
    let icon = gtk::Image::from_icon_name(file_icon(&file.content_type, &file.name));
    icon.set_valign(gtk::Align::Center);

    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).hexpand(true).build();
    text.append(&gtk::Label::builder().label(&file.name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build());
    let meta = format!("{} · {}", human_size(file.size), file.date.with_timezone(&chrono::Local).format("%d/%m/%Y"));
    text.append(&gtk::Label::builder().label(&meta).xalign(0.0).css_classes(["caption", "dim-label"]).build());

    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(&icon);
    row.append(&text);

    let button = gtk::Button::builder().child(&row).css_classes(["flat", "profile-card-row"]).build();
    // The file name/size/date are content: blurred by privacy mode.
    crate::privacy::mark_content(&button);
    button.set_tooltip_text(Some("Open the message with this file"));
    let actions = actions.clone();
    let message = file.message.clone();
    button.connect_clicked(move |_| (actions.open_message)(&message));
    button.upcast()
}

/// A themed icon for a file's MIME type / name, falling back to a generic one.
fn file_icon(content_type: &str, name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if content_type.starts_with("image/") {
        "image-x-generic-symbolic"
    } else if content_type == "application/pdf" || lower.ends_with(".pdf") {
        "x-office-document-symbolic"
    } else if content_type.starts_with("text/") {
        "text-x-generic-symbolic"
    } else if content_type.starts_with("audio/") {
        "audio-x-generic-symbolic"
    } else if content_type.starts_with("video/") {
        "video-x-generic-symbolic"
    } else {
        "mail-attachment-symbolic"
    }
}

/// A human-readable byte size ("12.4 kB").
fn human_size(bytes: u32) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The caches in scope for a contact: the one account's cache when known, or
/// every account's otherwise.
fn account_caches(state: &Rc<RefCell<UiState>>, account_id: &lookout_core::AccountId) -> Vec<std::sync::Arc<lookout_mail::Cache>> {
    let st = state.borrow();
    st.accounts.get(account_id).and_then(|handle| handle.address_cache.clone()).into_iter().collect()
}

fn section_heading(text: &str) -> gtk::Widget {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .margin_top(2)
        .build()
        .upcast()
}

/// A selectable value line.
fn info_value(text: &str) -> gtk::Widget {
    let label = gtk::Label::builder().label(text).xalign(0.0).wrap(true).selectable(true).css_classes(["caption"]).build();
    crate::privacy::mark(&label);
    label.upcast()
}

fn empty_label(text: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption", "dim-label"])
        .margin_top(2)
        .margin_bottom(2)
        .build();
    crate::privacy::mark_ui(&label);
    label.upcast()
}

/// The line form of a vCard address, omitting empty components.
fn format_address(address: &lookout_core::AddressField) -> String {
    [&address.street, &address.locality, &address.region, &address.postal_code, &address.country]
        .into_iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_contact_date(value: lookout_core::Birthday) -> String {
    if value.omit_year {
        value.date.format("%m-%d").to_string()
    } else {
        value.date.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_scales_to_the_right_unit() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 kB");
        assert_eq!(human_size(12_400), "12.4 kB");
        assert_eq!(human_size(2_500_000), "2.5 MB");
    }

    #[test]
    fn file_icon_follows_the_content_type_then_the_name() {
        assert_eq!(file_icon("image/png", "photo.png"), "image-x-generic-symbolic");
        assert_eq!(file_icon("application/octet-stream", "report.pdf"), "x-office-document-symbolic");
        assert_eq!(file_icon("text/plain", "notes.txt"), "text-x-generic-symbolic");
        assert_eq!(file_icon("application/zip", "bundle.zip"), "mail-attachment-symbolic");
    }
}
