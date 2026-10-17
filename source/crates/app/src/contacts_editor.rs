//! Contact editor (create/edit/delete) for the People module - the CardDAV
//! counterpart of `event_editor.rs`'s event dialog.
//!
//! Two consumers share one form: the modal dialog here (`show_contact_editor`,
//! used for "New contact" and the pop-out window), and the People screen's
//! details pane, which edits a contact in place (UX-65). [`build_contact_form`]
//! is the reusable half - it builds the field rows and returns a closure that
//! reads them back into a [`VCard`] - and the dialog just wraps it with a
//! footer.
//!
//! Data-in/callback-out like the event editor: the caller supplies the
//! pickable address books (create mode), the contact being edited (if any)
//! with its server-side `href`/`etag`, then registers `on_save` (create),
//! `on_update` (edit in place) and `on_delete` callbacks - the form never
//! talks to the network itself; the caller routes the produced [`VCard`] to
//! the account's CardDAV command channel. The editor starts from a clone of
//! the existing card and rewrites the fields it exposes (including
//! `URL`/`IMPP`/`ANNIVERSARY`/`PHOTO` and arbitrary custom properties), so a
//! `KIND:group` membership or a property the form has no row for still
//! survives an edit.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AddressField, EmailField, ImppField, OtherProperty, Photo, TelephoneField, VCard};

/// The create callback's target: the picked book's href and the card.
type CreateCallback = Rc<dyn Fn(String, VCard)>;
/// The update callback's target: the card's own href/etag plus the card.
type UpdateCallback = Rc<dyn Fn(String, Option<String>, VCard)>;
/// The delete callback's target: the resource to remove.
type DeleteCallback = Rc<dyn Fn(String, Option<String>)>;

/// The reusable contact form: its field body (without a footer) plus the
/// closure that reads the fields back into a card. The address-book picker is
/// present only in create mode.
pub struct ContactForm {
    /// Every field row, ready to drop into a scroller or a pane.
    pub body: gtk::Box,
    /// Builds the card from the current field values; `None` on invalid input
    /// (no name, bad date), in which case the caller just doesn't save.
    pub build_card: Rc<dyn Fn() -> Option<VCard>>,
    /// The address-book picker (create mode only).
    pub book_dropdown: Option<gtk::DropDown>,
    /// Parallel hrefs for `book_dropdown`'s entries.
    pub book_hrefs: Vec<String>,
}

/// What to prefill the editor with.
pub struct ContactEditorPrefill<'a> {
    /// `(display label, book href)` for the address-book picker, in picker
    /// order. Empty disables the picker (create mode with no writable book).
    pub books: &'a [(String, String)],
    /// The contact being edited; `None` opens the blank "new contact" form.
    pub existing: Option<&'a VCard>,
    /// Field values to seed a *create* form with (the sender's name/email
    /// when the reading pane's "View contact" finds no match). Ignored when
    /// `existing` is set - an edit always starts from the real card.
    pub prefill: Option<&'a VCard>,
    /// The existing contact's server-side object href. Empty marks the entry
    /// as not writable (the Deleted bucket's display-only cards), which hides
    /// the Save/Delete actions.
    pub href: &'a str,
    /// The existing contact's `getetag`, for the `If-Match` update guard.
    pub etag: Option<&'a str>,
}

/// The type choices for a dynamic email/phone row.
const FIELD_TYPES: [&str; 3] = ["work", "home", "other"];

/// One dynamic email/phone/address row: its text entries, type dropdown and
/// remove button, kept by identity so rows can be removed and read back
/// without walking the widget tree.
struct FieldRow {
    boxed: gtk::Box,
    /// The row's main text entry: email address, phone number, street, URL,
    /// IM URI, or a custom property's name.
    entry: gtk::Entry,
    /// The row's optional second entry: an address's city, or a custom
    /// property's value; `None` for email/phone/URL/IM rows.
    city_entry: Option<gtk::Entry>,
    dropdown: gtk::DropDown,
    remove: gtk::Button,
}

/// Builds one dynamic email/phone/IM row: a text entry, a `TYPE` dropdown
/// (`work` / `home` / `other`) and a remove button.
fn build_field_row(initial: &str, initial_type: Option<&str>) -> FieldRow {
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();

    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_hexpand(true);
    boxed.append(&entry);

    let type_list = gtk::StringList::new(&FIELD_TYPES);
    let dropdown = gtk::DropDown::builder().model(&type_list).valign(gtk::Align::Center).build();
    let selected = initial_type
        .and_then(|t| FIELD_TYPES.iter().position(|candidate| candidate.eq_ignore_ascii_case(t)))
        .unwrap_or(0);
    dropdown.set_selected(selected as u32);
    boxed.append(&dropdown);

    let remove = remove_button();
    boxed.append(&remove);

    FieldRow {
        boxed,
        entry,
        city_entry: None,
        dropdown,
        remove,
    }
}

/// Builds an address row: a street entry, a city entry and a remove button.
fn build_address_row(street: &str, city: &str) -> FieldRow {
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let street_entry = gtk::Entry::new();
    street_entry.set_placeholder_text(Some("Street"));
    street_entry.set_text(street);
    street_entry.set_hexpand(true);
    boxed.append(&street_entry);
    let city_entry = gtk::Entry::new();
    city_entry.set_placeholder_text(Some("City"));
    city_entry.set_text(city);
    city_entry.set_width_chars(14);
    boxed.append(&city_entry);
    let remove = remove_button();
    boxed.append(&remove);
    let type_list = gtk::StringList::new(&FIELD_TYPES);
    let dropdown = gtk::DropDown::builder().model(&type_list).valign(gtk::Align::Center).build();
    dropdown.set_visible(false);
    boxed.append(&dropdown);
    FieldRow {
        boxed,
        entry: street_entry,
        city_entry: Some(city_entry),
        dropdown,
        remove,
    }
}

/// Builds a website row: one URL entry, with the shared row's type dropdown
/// hidden (the model's `URL` is a bare string).
fn build_url_row(url: &str) -> FieldRow {
    let row = build_field_row(url, None);
    row.dropdown.set_visible(false);
    row.entry.set_placeholder_text(Some("https://example.com"));
    row
}

/// Builds a custom-property row: a name entry and a value entry. Reuses the
/// shared row shape as `entry` (name) plus `city_entry` (value) so the remove
/// wiring works unchanged.
fn build_custom_row(name: &str, value: &str) -> FieldRow {
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let name_entry = gtk::Entry::new();
    name_entry.set_placeholder_text(Some("Field"));
    name_entry.set_text(name);
    name_entry.set_width_chars(14);
    boxed.append(&name_entry);
    let value_entry = gtk::Entry::new();
    value_entry.set_placeholder_text(Some("Value"));
    value_entry.set_text(value);
    value_entry.set_hexpand(true);
    boxed.append(&value_entry);
    let remove = remove_button();
    boxed.append(&remove);
    let type_list = gtk::StringList::new(&FIELD_TYPES);
    let dropdown = gtk::DropDown::builder().model(&type_list).valign(gtk::Align::Center).build();
    dropdown.set_visible(false);
    boxed.append(&dropdown);
    FieldRow {
        boxed,
        entry: name_entry,
        city_entry: Some(value_entry),
        dropdown,
        remove,
    }
}

/// The flat "Remove" button every dynamic row carries.
fn remove_button() -> gtk::Button {
    let remove = gtk::Button::from_icon_name("user-trash-symbolic");
    remove.set_css_classes(&["flat"]);
    remove.set_valign(gtk::Align::Center);
    remove.set_tooltip_text(Some("Remove"));
    remove
}

/// Builds the reusable contact form. `seed` is the card to prefill (the
/// existing contact, or a create-mode prefill); `books` enables the
/// address-book picker, locked when `lock_book` is set (editing an existing
/// contact).
pub fn build_contact_form(seed: Option<&VCard>, books: Option<&[(String, String)]>, lock_book: bool) -> ContactForm {
    let seed: Option<VCard> = seed.cloned();

    // --- Address-book picker (create mode only): a `StringList` of labels
    // plus a parallel href vector, the same pattern as the event editor's
    // calendar picker. Locked while editing - moving a contact between books
    // is out of scope for this pass.
    let book_labels: Vec<String> = books.map(|books| books.iter().map(|(label, _)| label.clone()).collect()).unwrap_or_default();
    let book_hrefs: Vec<String> = books.map(|books| books.iter().map(|(_, href)| href.clone()).collect()).unwrap_or_default();
    let label_refs: Vec<&str> = book_labels.iter().map(String::as_str).collect();
    let string_list = gtk::StringList::new(&label_refs);
    let book_dropdown = if books.is_some() {
        Some(
            gtk::DropDown::builder()
                .model(&string_list)
                .selected(if book_hrefs.is_empty() { u32::MAX } else { 0 })
                .sensitive(!lock_book)
                .valign(gtk::Align::Center)
                .build(),
        )
    } else {
        None
    };
    if let Some(dropdown) = &book_dropdown {
        // Address-book names are content: blurred by privacy mode.
        crate::privacy::mark_content(dropdown);
    }

    // --- Single-valued fields.
    let name_row = adw::EntryRow::new();
    name_row.set_title("Full name");
    let org_row = adw::EntryRow::new();
    org_row.set_title("Organization");
    let title_row = adw::EntryRow::new();
    title_row.set_title("Title");
    // P-07: the per-contact "File as" override. Empty means the contact is
    // filed by the global preference (Settings → People).
    let file_as_row = adw::EntryRow::new();
    file_as_row.set_title("File as");
    let birthday_row = adw::EntryRow::new();
    birthday_row.set_title("Birthday");
    let anniversary_row = adw::EntryRow::new();
    anniversary_row.set_title("Anniversary");
    let groups_row = adw::EntryRow::new();
    groups_row.set_title("Groups");
    let notes_entry = adw::EntryRow::new();
    notes_entry.set_title("Notes");
    notes_entry.set_show_apply_button(true);

    let emails: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let phones: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let addresses: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let websites: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let ims: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let customs: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));

    let emails_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let phones_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let addresses_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let websites_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let ims_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let customs_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();

    // Every row's remove button removes that row from its container and
    // vector, matched by widget identity (the row struct itself is owned by
    // the vector).
    let wire_remove = {
        |row: &FieldRow, rows: &Rc<RefCell<Vec<FieldRow>>>, container: &gtk::Box| {
            let rows = rows.clone();
            let container = container.clone();
            let boxed = row.boxed.clone();
            row.remove.connect_clicked(move |_| {
                let mut rows = rows.borrow_mut();
                rows.retain(|candidate| candidate.boxed != boxed);
                drop(rows);
                container.remove(&boxed);
            });
        }
    };

    // --- Photo: the contact's `PHOTO`, previewed and replaced/cleared from
    // the form. The held `Photo` is written back on save; a card that carried
    // only a photo URI keeps it unless the user sets or removes a photo.
    let photo_state: Rc<RefCell<Option<Photo>>> = Rc::new(RefCell::new(seed.as_ref().and_then(|card| card.photo.clone())));
    let photo_picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .width_request(96)
        .height_request(96)
        .css_classes(["contact-photo"])
        .build();
    let refresh_photo: Rc<dyn Fn()> = {
        let photo_state = photo_state.clone();
        let photo_picture = photo_picture.clone();
        Rc::new(move || {
            let texture = photo_state
                .borrow()
                .as_ref()
                .and_then(|photo| photo.data.clone())
                .and_then(|data| gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)).ok());
            match texture {
                Some(texture) => photo_picture.set_paintable(Some(&texture)),
                None => photo_picture.set_paintable(None::<&gtk::gdk::Texture>),
            }
        })
    };
    refresh_photo();

    // --- Repopulate from the existing card (or the create prefill).
    if let Some(card) = &seed {
        name_row.set_text(card.full_name.as_deref().unwrap_or(""));
        org_row.set_text(card.organization.as_ref().and_then(|org| org.first()).map(String::as_str).unwrap_or(""));
        title_row.set_text(card.title.as_deref().unwrap_or(""));
        file_as_row.set_text(card.file_as.as_deref().unwrap_or(""));
        if let Some(birthday) = card.birthday {
            if birthday.omit_year {
                birthday_row.set_text(&birthday.date.format("%m-%d").to_string());
            } else {
                birthday_row.set_text(&birthday.date.format("%Y-%m-%d").to_string());
            }
        }
        if let Some(anniversary) = card.anniversary {
            if anniversary.omit_year {
                anniversary_row.set_text(&anniversary.date.format("%m-%d").to_string());
            } else {
                anniversary_row.set_text(&anniversary.date.format("%Y-%m-%d").to_string());
            }
        }
        groups_row.set_text(&card.categories.join(", "));
        notes_entry.set_text(card.note.as_deref().unwrap_or(""));
        for email in &card.emails {
            let row = build_field_row(&email.address, email.types.first().map(String::as_str));
            wire_remove(&row, &emails, &emails_box);
            emails_box.append(&row.boxed);
            emails.borrow_mut().push(row);
        }
        for phone in &card.telephones {
            let row = build_field_row(&phone.number, phone.types.first().map(String::as_str));
            wire_remove(&row, &phones, &phones_box);
            phones_box.append(&row.boxed);
            phones.borrow_mut().push(row);
        }
        for address in &card.addresses {
            let row = build_address_row(address.street.as_str(), address.locality.as_str());
            wire_remove(&row, &addresses, &addresses_box);
            addresses_box.append(&row.boxed);
            addresses.borrow_mut().push(row);
        }
        for url in &card.urls {
            let row = build_url_row(url);
            wire_remove(&row, &websites, &websites_box);
            websites_box.append(&row.boxed);
            websites.borrow_mut().push(row);
        }
        for impp in &card.impp {
            let row = build_field_row(&impp.uri, impp.types.first().map(String::as_str));
            wire_remove(&row, &ims, &ims_box);
            ims_box.append(&row.boxed);
            ims.borrow_mut().push(row);
        }
        for property in &card.other {
            let row = build_custom_row(&property.name, &property.value);
            wire_remove(&row, &customs, &customs_box);
            customs_box.append(&row.boxed);
            customs.borrow_mut().push(row);
        }
    }

    // --- Add-row buttons.
    let add_email = gtk::Button::with_label("Add email");
    add_email.connect_clicked({
        let emails = emails.clone();
        let emails_box = emails_box.clone();
        move |_| {
            let row = build_field_row("", None);
            wire_remove(&row, &emails, &emails_box);
            emails_box.append(&row.boxed);
            emails.borrow_mut().push(row);
        }
    });

    let add_phone = gtk::Button::with_label("Add phone");
    add_phone.connect_clicked({
        let phones = phones.clone();
        let phones_box = phones_box.clone();
        move |_| {
            let row = build_field_row("", None);
            wire_remove(&row, &phones, &phones_box);
            phones_box.append(&row.boxed);
            phones.borrow_mut().push(row);
        }
    });

    let add_address = gtk::Button::with_label("Add address");
    add_address.connect_clicked({
        let addresses = addresses.clone();
        let addresses_box = addresses_box.clone();
        move |_| {
            let row = build_address_row("", "");
            wire_remove(&row, &addresses, &addresses_box);
            addresses_box.append(&row.boxed);
            addresses.borrow_mut().push(row);
        }
    });

    let add_website = gtk::Button::with_label("Add website");
    add_website.connect_clicked({
        let websites = websites.clone();
        let websites_box = websites_box.clone();
        move |_| {
            let row = build_url_row("");
            wire_remove(&row, &websites, &websites_box);
            websites_box.append(&row.boxed);
            websites.borrow_mut().push(row);
        }
    });

    let add_im = gtk::Button::with_label("Add IM");
    add_im.connect_clicked({
        let ims = ims.clone();
        let ims_box = ims_box.clone();
        move |_| {
            let row = build_field_row("", None);
            wire_remove(&row, &ims, &ims_box);
            ims_box.append(&row.boxed);
            ims.borrow_mut().push(row);
        }
    });

    let add_custom = gtk::Button::with_label("Add field");
    add_custom.connect_clicked({
        let customs = customs.clone();
        let customs_box = customs_box.clone();
        move |_| {
            let row = build_custom_row("", "");
            wire_remove(&row, &customs, &customs_box);
            customs_box.append(&row.boxed);
            customs.borrow_mut().push(row);
        }
    });

    // --- Photo controls: pick an image file, or clear the current photo.
    let set_photo_button = gtk::Button::with_label("Set photo…");
    set_photo_button.connect_clicked({
        let photo_state = photo_state.clone();
        let refresh_photo = refresh_photo.clone();
        move |button| {
            let parent = button.root().and_then(|root| root.downcast::<gtk::Window>().ok());
            let photo_state = photo_state.clone();
            let refresh_photo = refresh_photo.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_pixbuf_formats();
                filter.set_name(Some("Images"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let picker = gtk::FileDialog::builder().title("Choose a photo").filters(&filters).build();
                let Ok(file) = picker.open_future(parent.as_ref()).await else { return };
                let Ok((bytes, _etag)) = file.load_contents_future().await else { return };
                let media_type = file
                    .query_info_future("standard::content-type", gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT)
                    .await
                    .ok()
                    .and_then(|info| info.content_type())
                    .and_then(|content_type| gio::functions::content_type_get_mime_type(&content_type))
                    .map(|mime| mime.to_string());
                *photo_state.borrow_mut() = Some(Photo {
                    media_type,
                    data: Some(bytes.to_vec()),
                    uri: None,
                });
                refresh_photo();
            });
        }
    });
    let remove_photo_button = gtk::Button::with_label("Remove photo");
    remove_photo_button.connect_clicked({
        let photo_state = photo_state.clone();
        let refresh_photo = refresh_photo.clone();
        move |_| {
            *photo_state.borrow_mut() = None;
            refresh_photo();
        }
    });

    // --- Assemble the form body.
    let content_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let photo_buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Center).build();
    photo_buttons.append(&set_photo_button);
    photo_buttons.append(&remove_photo_button);
    let photo_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    photo_row.append(&photo_picture);
    photo_row.append(&photo_buttons);

    if let Some(book_dropdown) = &book_dropdown {
        let book_row = adw::ActionRow::builder().title("Address book").build();
        book_row.add_suffix(book_dropdown);
        content_box.append(&book_row);
    }
    content_box.append(&section_header("Photo"));
    content_box.append(&photo_row);
    content_box.append(&name_row);
    content_box.append(&org_row);
    content_box.append(&title_row);
    content_box.append(&file_as_row);
    content_box.append(&birthday_row);
    content_box.append(&anniversary_row);
    content_box.append(&section_header("Emails"));
    content_box.append(&emails_box);
    content_box.append(&add_email);
    content_box.append(&section_header("Phones"));
    content_box.append(&phones_box);
    content_box.append(&add_phone);
    content_box.append(&section_header("Addresses"));
    content_box.append(&addresses_box);
    content_box.append(&add_address);
    content_box.append(&section_header("Websites"));
    content_box.append(&websites_box);
    content_box.append(&add_website);
    content_box.append(&section_header("Instant messaging"));
    content_box.append(&ims_box);
    content_box.append(&add_im);
    content_box.append(&section_header("Custom fields"));
    content_box.append(&customs_box);
    content_box.append(&add_custom);
    content_box.append(&section_header("Notes"));
    content_box.append(&notes_entry);
    content_box.append(&groups_row);

    // --- Build the card from the form (clone of the seed card, only the
    // exposed fields rewritten). `None` on invalid input (no name, bad
    // birthday) - the caller just doesn't fire the save. A create seeded from
    // a prefill keeps that card's own UID (the caller's prefilled cards carry
    // one); the blank form generates a fresh one.
    let build_card: Rc<dyn Fn() -> Option<VCard>> = Rc::new(move || {
        let mut card = seed.clone().unwrap_or_else(|| VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: Some(uuid::Uuid::new_v4().to_string()),
            full_name: None,
            name: None,
            organization: None,
            title: None,
            file_as: None,
            photo: None,
            emails: Vec::new(),
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            impp: Vec::new(),
            note: None,
            birthday: None,
            anniversary: None,
            categories: Vec::new(),
            other: Vec::new(),
        });

        let full_name = name_row.text().trim().to_string();
        if full_name.is_empty() {
            return None;
        }
        card.full_name = Some(full_name);

        let org = org_row.text().trim().to_string();
        card.organization = if org.is_empty() { None } else { Some(vec![org]) };
        let title = title_row.text().trim().to_string();
        card.title = if title.is_empty() { None } else { Some(title) };

        let file_as = file_as_row.text().trim().to_string();
        card.file_as = if file_as.is_empty() { None } else { Some(file_as) };

        let mut emails_out = Vec::new();
        for row in emails.borrow().iter() {
            let address = row.entry.text().trim().to_string();
            if address.is_empty() {
                continue;
            }
            let types = vec![FIELD_TYPES[row.dropdown.selected() as usize].to_string()];
            emails_out.push(EmailField { types, address });
        }
        card.emails = emails_out;

        let mut phones_out = Vec::new();
        for row in phones.borrow().iter() {
            let number = row.entry.text().trim().to_string();
            if number.is_empty() {
                continue;
            }
            let types = vec![FIELD_TYPES[row.dropdown.selected() as usize].to_string()];
            phones_out.push(TelephoneField { types, number });
        }
        card.telephones = phones_out;

        let mut addresses_out = Vec::new();
        for row in addresses.borrow().iter() {
            let street = row.entry.text().trim().to_string();
            let city = row.city_entry.as_ref().map(|entry| entry.text().trim().to_string()).unwrap_or_default();
            if street.is_empty() && city.is_empty() {
                continue;
            }
            addresses_out.push(AddressField {
                types: Vec::new(),
                po_box: String::new(),
                extended: String::new(),
                street,
                locality: city,
                region: String::new(),
                postal_code: String::new(),
                country: String::new(),
                label: None,
            });
        }
        card.addresses = addresses_out;

        let note = notes_entry.text().trim().to_string();
        card.note = if note.is_empty() { None } else { Some(note) };

        // Invalid date input fails the whole save (the caller just doesn't
        // fire), matching the birthday behaviour.
        card.birthday = match parse_date_entry(birthday_row.text().as_str()) {
            Some(value) => value,
            None => return None,
        };
        card.anniversary = match parse_date_entry(anniversary_row.text().as_str()) {
            Some(value) => value,
            None => return None,
        };

        card.urls = websites
            .borrow()
            .iter()
            .map(|row| row.entry.text().trim().to_string())
            .filter(|url| !url.is_empty())
            .collect();

        let mut impp_out = Vec::new();
        for row in ims.borrow().iter() {
            let uri = row.entry.text().trim().to_string();
            if uri.is_empty() {
                continue;
            }
            let types = vec![FIELD_TYPES[row.dropdown.selected() as usize].to_string()];
            impp_out.push(ImppField { types, uri });
        }
        card.impp = impp_out;

        // Custom properties: the row's name identifies the property, and its
        // original `Parameter`s are carried over from the card being edited
        // (matched by name) so an `X-*;TYPE=...` value isn't flattened.
        let original_custom = card.other.clone();
        let mut other_out = Vec::new();
        for row in customs.borrow().iter() {
            let name = row.entry.text().trim().to_string();
            if name.is_empty() {
                continue;
            }
            let value = row.city_entry.as_ref().map(|entry| entry.text().trim().to_string()).unwrap_or_default();
            let params = original_custom
                .iter()
                .find(|property| property.name.eq_ignore_ascii_case(&name))
                .map(|property| property.params.clone())
                .unwrap_or_default();
            other_out.push(OtherProperty { name, params, value });
        }
        card.other = other_out;

        card.categories = groups_row.text().split(',').map(str::trim).filter(|part| !part.is_empty()).map(str::to_string).collect();

        card.photo = photo_state.borrow().clone();

        Some(card)
    });

    ContactForm {
        body: content_box,
        build_card,
        book_dropdown,
        book_hrefs,
    }
}

/// Opens the contact editor as a modal dialog. `on_save` fires with the
/// picked book's href and the finished card (new contact); `on_update` with
/// the card's href/etag plus the edited card; `on_delete` with the delete
/// target. All are responsible for routing to the owning account's session -
/// the dialog only builds and validates the card.
pub fn show_contact_editor(
    window: &adw::ApplicationWindow,
    prefill: ContactEditorPrefill,
    on_save: impl Fn(String, VCard) + 'static,
    on_update: impl Fn(String, Option<String>, VCard) + 'static,
    on_delete: impl Fn(String, Option<String>) + 'static,
) {
    // Boxed as trait objects for the same reason `event_editor.rs` does:
    // a closure capturing an `impl Fn` parameter can't implement `Fn` itself.
    let on_save: CreateCallback = Rc::new(on_save);
    let on_update: UpdateCallback = Rc::new(on_update);
    let on_delete: DeleteCallback = Rc::new(on_delete);

    let has_existing = prefill.existing.is_some();
    // The form's seed: the real card for an edit, the caller's create-mode
    // prefill otherwise - so a prefilled "new contact" (the reading pane's
    // "View contact" create path) opens with its name/email rows populated.
    let seed: Option<VCard> = prefill.existing.cloned().or_else(|| prefill.prefill.cloned());
    let href_owned = prefill.href.to_string();
    let etag_owned = prefill.etag.map(str::to_string);
    // Save is only possible when there's a write target: a real href for an
    // edit, at least one book for a create.
    let writable = (has_existing && !href_owned.is_empty()) || (!has_existing && !prefill.books.is_empty());

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title(if has_existing { "Edit contact" } else { "New contact" })
        .default_width(560)
        .default_height(640)
        .build();

    let form = build_contact_form(seed.as_ref(), Some(prefill.books), has_existing);

    let scroller = gtk::ScrolledWindow::builder()
        .child(&form.body)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&scroller);

    // --- Footer.
    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label(if has_existing { "Save" } else { "Create" });
    save_button.add_css_class("suggested-action");
    save_button.set_sensitive(writable);
    let delete_button = gtk::Button::with_label("Delete");
    delete_button.add_css_class("destructive-action");
    delete_button.set_sensitive(has_existing && !href_owned.is_empty());

    let footer = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    footer.set_margin_top(6);
    footer.set_margin_bottom(12);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_halign(gtk::Align::End);
    footer.append(&cancel_button);
    if has_existing {
        footer.prepend(&delete_button);
    }
    footer.append(&save_button);
    content.append(&footer);

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    let on_save = on_save.clone();
    let on_update = on_update.clone();
    let build_card = form.build_card.clone();
    let book_dropdown = form.book_dropdown.clone();
    let book_hrefs = form.book_hrefs.clone();
    save_button.connect_clicked({
        let dialog = dialog.clone();
        let href_owned = href_owned.clone();
        let etag_owned = etag_owned.clone();
        move |_| {
            let Some(card) = build_card() else { return };
            if has_existing {
                on_update(href_owned.clone(), etag_owned.clone(), card);
            } else {
                let book_href = book_dropdown
                    .as_ref()
                    .and_then(|dropdown| book_hrefs.get(dropdown.selected() as usize))
                    .cloned()
                    .unwrap_or_default();
                on_save(book_href, card);
            }
            dialog.close();
        }
    });

    let on_delete = on_delete.clone();
    delete_button.connect_clicked({
        let dialog = dialog.clone();
        let href_owned = href_owned.clone();
        let etag_owned = etag_owned.clone();
        move |_| {
            on_delete(href_owned.clone(), etag_owned.clone());
            dialog.close();
        }
    });

    dialog.set_content(Some(&content));
    dialog.present();
}

/// Parses the Birthday/Anniversary entry text into a [`lookout_core::Birthday`].
/// An empty string clears the field (`Some(None)`); a full `YYYY-MM-DD` date
/// clears any year-omission; `MM-DD` marks the date yearless (it recurs every
/// year); anything else is invalid (`None`) and fails the save.
fn parse_date_entry(text: &str) -> Option<Option<lookout_core::Birthday>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(None);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(Some(lookout_core::Birthday { date, omit_year: false }));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%m-%d") {
        return Some(Some(lookout_core::Birthday { date, omit_year: true }));
    }
    None
}

fn section_header(label: &str) -> gtk::Label {
    let header = gtk::Label::builder().label(label).xalign(0.0).css_classes(["heading"]).build();
    crate::privacy::mark_ui(&header);
    header.set_margin_top(4);
    header
}
