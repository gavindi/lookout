//! Modal contact editor (create/edit/delete) for the People module - the
//! CardDAV counterpart of `event_editor.rs`'s event dialog.
//!
//! Data-in/callback-out like the event editor: the caller supplies the
//! pickable address books (create mode), the contact being edited (if any)
//! with its server-side `href`/`etag`, then registers `on_save` (create),
//! `on_update` (edit in place) and `on_delete` callbacks - the dialog never
//! talks to the network itself; the caller routes the produced [`VCard`] to
//! the account's CardDAV command channel. Unedited fields ride along
//! untouched: the editor starts from a clone of the existing card and only
//! rewrites the fields it exposes, so properties the form doesn't know about
//! (custom `X-` props, `KIND:group` membership, URLs, ...) survive an edit.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use lookout_core::{AddressField, EmailField, TelephoneField, VCard};

/// The create callback's target: the picked book's href and the card.
type CreateCallback = Rc<dyn Fn(String, VCard)>;
/// The update callback's target: the card's own href/etag plus the card.
type UpdateCallback = Rc<dyn Fn(String, Option<String>, VCard)>;
/// The delete callback's target: the resource to remove.
type DeleteCallback = Rc<dyn Fn(String, Option<String>)>;

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
    /// The row's main text entry: email address, phone number, or street.
    entry: gtk::Entry,
    /// The address row's second entry (city); `None` for email/phone rows.
    city_entry: Option<gtk::Entry>,
    dropdown: gtk::DropDown,
    remove: gtk::Button,
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

    let existing: Option<VCard> = prefill.existing.cloned();
    let has_existing = existing.is_some();
    // The form's seed: the real card for an edit, the caller's create-mode
    // prefill otherwise - so a prefilled "new contact" (the reading pane's
    // "View contact" create path) opens with its name/email rows populated.
    let seed: Option<VCard> = existing.clone().or_else(|| prefill.prefill.cloned());
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

    // --- Address-book picker (create mode only): a `StringList` of labels
    // plus a parallel href vector, the same pattern as the event editor's
    // calendar picker. Locked while editing - moving a contact between books
    // is out of scope for this pass.
    let book_labels: Vec<String> = prefill.books.iter().map(|(label, _)| label.clone()).collect();
    let book_hrefs: Vec<String> = prefill.books.iter().map(|(_, href)| href.clone()).collect();
    let label_refs: Vec<&str> = book_labels.iter().map(String::as_str).collect();
    let string_list = gtk::StringList::new(&label_refs);
    let book_dropdown = gtk::DropDown::builder()
        .model(&string_list)
        .selected(if book_hrefs.is_empty() { u32::MAX } else { 0 })
        .sensitive(!has_existing)
        .valign(gtk::Align::Center)
        .build();
    let book_row = adw::ActionRow::builder().title("Address book").build();
    book_row.add_suffix(&book_dropdown);

    // --- Single-valued fields.
    let name_row = adw::EntryRow::new();
    name_row.set_title("Full name");
    let org_row = adw::EntryRow::new();
    org_row.set_title("Organization");
    let title_row = adw::EntryRow::new();
    title_row.set_title("Title");
    let birthday_row = adw::EntryRow::new();
    birthday_row.set_title("Birthday");
    let groups_row = adw::EntryRow::new();
    groups_row.set_title("Groups");
    let notes_entry = adw::EntryRow::new();
    notes_entry.set_title("Notes");
    notes_entry.set_show_apply_button(true);

    // --- Dynamic email/phone/address rows.
    let build_field_row = |initial: &str, initial_type: Option<&str>| -> FieldRow {
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

        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_css_classes(&["flat"]);
        remove.set_valign(gtk::Align::Center);
        remove.set_tooltip_text(Some("Remove"));
        boxed.append(&remove);

        FieldRow {
            boxed,
            entry,
            city_entry: None,
            dropdown,
            remove,
        }
    };

    let build_address_row = |street: &str, city: &str| -> FieldRow {
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
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_css_classes(&["flat"]);
        remove.set_valign(gtk::Align::Center);
        remove.set_tooltip_text(Some("Remove"));
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
    };

    let emails: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let phones: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));
    let addresses: Rc<RefCell<Vec<FieldRow>>> = Rc::new(RefCell::new(Vec::new()));

    let emails_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let phones_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let addresses_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();

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

    // --- Repopulate from the existing card (or the create prefill).
    if let Some(card) = &seed {
        name_row.set_text(card.full_name.as_deref().unwrap_or(""));
        org_row.set_text(card.organization.as_ref().and_then(|org| org.first()).map(String::as_str).unwrap_or(""));
        title_row.set_text(card.title.as_deref().unwrap_or(""));
        if let Some(birthday) = card.birthday {
            if birthday.omit_year {
                birthday_row.set_text(&birthday.date.format("%m-%d").to_string());
            } else {
                birthday_row.set_text(&birthday.date.format("%Y-%m-%d").to_string());
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

    // --- Assemble the form.
    let content_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content_box)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&scroller);

    content_box.append(&book_row);
    content_box.append(&name_row);
    content_box.append(&org_row);
    content_box.append(&title_row);
    content_box.append(&birthday_row);
    content_box.append(&section_header("Emails"));
    content_box.append(&emails_box);
    content_box.append(&add_email);
    content_box.append(&section_header("Phones"));
    content_box.append(&phones_box);
    content_box.append(&add_phone);
    content_box.append(&section_header("Addresses"));
    content_box.append(&addresses_box);
    content_box.append(&add_address);
    content_box.append(&section_header("Notes"));
    content_box.append(&notes_entry);
    content_box.append(&groups_row);

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

    // --- Build the card from the form (clone of the existing/prefill card,
    // only the exposed fields rewritten). `None` on invalid input (no name,
    // bad birthday) - the Save handler just doesn't fire. A create seeded
    // from a prefill keeps that card's own UID (the caller's prefilled cards
    // carry one); the blank form generates a fresh one.
    let save_card: Rc<dyn Fn() -> Option<VCard>> = Rc::new(move || {
        let mut card = seed.clone().unwrap_or_else(|| VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: Some(uuid::Uuid::new_v4().to_string()),
            full_name: None,
            name: None,
            organization: None,
            title: None,
            emails: Vec::new(),
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            note: None,
            birthday: None,
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

        let birthday = birthday_row.text().trim().to_string();
        card.birthday = if birthday.is_empty() {
            None
        } else if let Ok(date) = chrono::NaiveDate::parse_from_str(&birthday, "%Y-%m-%d") {
            // A full date entered (or left untouched from a dated card)
            // clears any year-omission the card carried.
            Some(lookout_core::Birthday { date, omit_year: false })
        } else if let Ok(date) = chrono::NaiveDate::parse_from_str(&birthday, "%m-%d") {
            // Yearless form ("MM-DD", how yearless cards show in the entry):
            // the birthday recurs every year.
            Some(lookout_core::Birthday { date, omit_year: true })
        } else {
            // Invalid input - the Save handler just doesn't fire.
            return None;
        };

        card.categories = groups_row.text().split(',').map(str::trim).filter(|part| !part.is_empty()).map(str::to_string).collect();

        Some(card)
    });

    let on_save = on_save.clone();
    let on_update = on_update.clone();
    save_button.connect_clicked({
        let dialog = dialog.clone();
        let save_card = save_card.clone();
        let href_owned = href_owned.clone();
        let etag_owned = etag_owned.clone();
        move |_| {
            let Some(card) = save_card() else { return };
            if has_existing {
                on_update(href_owned.clone(), etag_owned.clone(), card);
            } else {
                let book_href = book_hrefs.get(book_dropdown.selected() as usize).cloned().unwrap_or_default();
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

fn section_header(label: &str) -> gtk::Label {
    let header = gtk::Label::builder().label(label).xalign(0.0).css_classes(["heading"]).build();
    header.set_margin_top(4);
    header
}
