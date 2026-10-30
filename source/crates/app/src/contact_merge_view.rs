//! People → Merge duplicates (OUTLOOK_PARITY.md item 4.6 / P-09).
//!
//! The People screen already keeps every synced contact in
//! `UiState::contacts_by_account`, and every write goes through the owning
//! account's `ContactCommand` channel. This module adds the missing review
//! layer on top of that data: it scans each account for likely copies, lets
//! the user choose one keeper and a value for every field, then updates the
//! keeper before deleting the other records.
//!
//! Matching and field merging are pure logic in
//! [`lookout_core::contact_merge`]; this file only adapts snapshots to that
//! API, renders the two-page dialog and drives the writes. Groups are matched
//! within one account, because deleting a copy from another provider's address
//! book is not a safe side effect of a merge.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::contact_merge::{completeness, default_choices, duplicate_groups, merge_cards, MatchKind, MatchReason, MergeChoices, MergeFields, MergeList, MergeScalar};
use lookout_core::{AccountId, VCard};

use crate::contacts_view::{contact_identity, dispatch_contact_command, vcard_display_name, vcard_primary_email, ContactCommand, ContactsAccountSnapshot, ContactsListEntry};
use crate::window::UiState;

/// The maximum number of characters of a value shown in a candidate row. The
/// full value stays in the row's tooltip.
const VALUE_PREVIEW_CHARS: usize = 140;

/// One duplicate group found in an account's current snapshot.
#[derive(Clone)]
pub(crate) struct ContactMergeGroup {
    pub(crate) account_label: String,
    pub(crate) entries: Vec<ContactsListEntry>,
    pub(crate) reasons: Vec<String>,
}

/// Scans the People screen's snapshots for duplicate groups.
///
/// Only writable, non-list records are candidates: the Deleted bucket and the
/// desktop's read-only books are not in `accounts` at all, and an empty
/// `href` marks a record that cannot be updated or deleted. Groups are built
/// per account and sorted for a stable dialog.
pub(crate) fn scan_groups(accounts: &HashMap<AccountId, ContactsAccountSnapshot>) -> Vec<ContactMergeGroup> {
    let mut ordered: Vec<(&AccountId, &ContactsAccountSnapshot)> = accounts.iter().collect();
    ordered.sort_by_key(|(_, snapshot)| snapshot.display_name().to_lowercase());

    let mut groups = Vec::new();
    for (account_id, snapshot) in ordered {
        let records: Vec<&lookout_dav::ContactRecord> = snapshot
            .contacts
            .iter()
            .filter(|record| !record.href.is_empty() && !record.card.is_contact_list())
            .collect();
        let cards: Vec<&VCard> = records.iter().map(|record| &record.card).collect();
        for group in duplicate_groups(&cards) {
            let mut entries: Vec<ContactsListEntry> = group
                .indices
                .iter()
                .filter_map(|index| records.get(*index))
                .map(|record| ContactsListEntry {
                    account_id: account_id.clone(),
                    account_label: snapshot.display_name().to_string(),
                    category_label: "Duplicates".to_string(),
                    record: Rc::new(lookout_dav::ContactRecord {
                        href: record.href.clone(),
                        etag: record.etag.clone(),
                        card: record.card.clone(),
                    }),
                })
                .collect();
            // The most complete card is the default keeper, so it leads the
            // group and `build_review_page` can start on index 0.
            entries.sort_by(|a, b| {
                completeness(b.card())
                    .cmp(&completeness(a.card()))
                    .then_with(|| vcard_display_name(a.card()).to_lowercase().cmp(&vcard_display_name(b.card()).to_lowercase()))
            });
            if entries.len() < 2 {
                continue;
            }
            let reasons = group.reasons.iter().map(reason_text).collect::<Vec<_>>();
            groups.push(ContactMergeGroup {
                account_label: snapshot.display_name().to_string(),
                entries,
                reasons,
            });
        }
    }

    groups.sort_by(|a, b| {
        a.account_label.to_lowercase().cmp(&b.account_label.to_lowercase()).then_with(|| {
            let a_name = a.entries.first().map(|entry| vcard_display_name(entry.card())).unwrap_or_default();
            let b_name = b.entries.first().map(|entry| vcard_display_name(entry.card())).unwrap_or_default();
            a_name.to_lowercase().cmp(&b_name.to_lowercase())
        })
    });
    groups
}

fn reason_text(reason: &MatchReason) -> String {
    match reason.kind {
        MatchKind::Email => format!("Shares {}", reason.value),
        MatchKind::Phone => format!("Shares {}", reason.value),
        MatchKind::Name => format!("Same name: {}", reason.value),
        MatchKind::Uid => "Same UID".to_string(),
    }
}

/// Shows the People screen's Merge duplicates dialog.
pub(crate) fn show_merge_duplicates_dialog(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay) {
    let groups = {
        let st = state.borrow();
        scan_groups(&st.contacts_by_account)
    };

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Merge duplicates")
        .default_width(780)
        .default_height(700)
        .build();
    let toolbar_view = adw::ToolbarView::new();

    let back_button = gtk::Button::from_icon_name("go-previous-symbolic");
    back_button.add_css_class("flat");
    back_button.set_tooltip_text(Some("Back"));
    back_button.update_property(&[gtk::accessible::Property::Label("Back")]);
    back_button.set_visible(false);

    let title_label = gtk::Label::builder().label("Merge duplicates").css_classes(["title"]).build();
    crate::privacy::mark_ui(&title_label);

    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    close_button.set_tooltip_text(Some("Close"));
    close_button.update_property(&[gtk::accessible::Property::Label("Close")]);
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }

    let top_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    top_bar.append(&back_button);
    top_bar.append(&title_label);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top_bar.append(&spacer);
    top_bar.append(&close_button);
    toolbar_view.add_top_bar(&top_bar);

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .transition_duration(180)
        .vexpand(true)
        .hexpand(true)
        .build();

    let review_host = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    stack.add_named(&review_host, Some("review"));

    let open_review: Rc<dyn Fn(usize)> = {
        let groups = groups.clone();
        let stack = stack.clone();
        let review_host = review_host.clone();
        let dialog = dialog.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let back_button = back_button.clone();
        let title_label = title_label.clone();
        Rc::new(move |index| {
            let Some(group) = groups.get(index).cloned() else { return };
            let on_merge: Rc<dyn Fn(usize, MergeChoices)> = {
                let group = group.clone();
                let dialog = dialog.clone();
                let state = state.clone();
                let toast_overlay = toast_overlay.clone();
                Rc::new(move |keeper, choices| present_merge_confirmation(&dialog, &state, &toast_overlay, &group, keeper, choices))
            };
            while let Some(child) = review_host.first_child() {
                review_host.remove(&child);
            }
            let page = build_review_page(&dialog, &group, 0, on_merge);
            review_host.append(&page);
            stack.set_visible_child_name("review");
            back_button.set_visible(true);
            title_label.set_text("Merge contacts");
        })
    };

    let groups_page = build_groups_page(&groups, open_review.clone());
    stack.add_named(&groups_page, Some("groups"));
    stack.set_visible_child_name("groups");

    {
        let stack = stack.clone();
        let back_for_click = back_button.clone();
        let title_label = title_label.clone();
        back_button.connect_clicked(move |_| {
            stack.set_visible_child_name("groups");
            back_for_click.set_visible(false);
            title_label.set_text("Merge duplicates");
        });
    }

    toolbar_view.set_content(Some(&stack));
    dialog.set_content(Some(&toolbar_view));
    dialog.present();
}

/// The groups page: one row per duplicate group with a Review button.
fn build_groups_page(groups: &[ContactMergeGroup], open_review: Rc<dyn Fn(usize)>) -> gtk::Widget {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    if groups.is_empty() {
        let status = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title("No duplicate contacts")
            .description("Contacts in the same account that share an email address, phone number or name will appear here.")
            .vexpand(true)
            .build();
        crate::privacy::mark_ui(&status);
        content.append(&status);
        let scroller = gtk::ScrolledWindow::builder().child(&content).vexpand(true).build();
        return scroller.upcast();
    }

    let heading = gtk::Label::builder()
        .label(format!("{} duplicate group{} found", groups.len(), if groups.len() == 1 { "" } else { "s" }))
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    crate::privacy::mark_ui(&heading);
    content.append(&heading);

    let group = adw::PreferencesGroup::builder()
        .title("Contacts to review")
        .description("Contacts are matched within one account. Choose one group to merge field by field.")
        .build();
    for (index, duplicate_group) in groups.iter().enumerate() {
        let name = duplicate_group
            .entries
            .first()
            .map(|entry| vcard_display_name(entry.card()))
            .unwrap_or_else(|| "Duplicate contacts".to_string());
        let count = duplicate_group.entries.len();
        let mut detail = format!("{count} contacts · {}", duplicate_group.account_label);
        if !duplicate_group.reasons.is_empty() {
            detail.push_str(" · ");
            detail.push_str(&duplicate_group.reasons.iter().take(2).cloned().collect::<Vec<_>>().join(" · "));
        }
        let row = adw::ActionRow::builder().title(name).subtitle(detail).build();
        crate::privacy::mark_row(&row);
        let review = gtk::Button::with_label("Review…");
        review.add_css_class("flat");
        review.set_tooltip_text(Some("Review this duplicate group"));
        review.update_property(&[gtk::accessible::Property::Label(&format!("Review duplicate group {}", index + 1))]);
        {
            let open_review = open_review.clone();
            review.connect_clicked(move |_| open_review(index));
        }
        row.add_suffix(&review);
        group.add(&row);
    }
    content.append(&group);

    let hint = gtk::Label::builder()
        .label("Merging updates the contact you keep, then deletes the other copies. The deletion is permanent.")
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .build();
    crate::privacy::mark_ui(&hint);
    content.append(&hint);

    let scroller = gtk::ScrolledWindow::builder().child(&content).vexpand(true).build();
    scroller.upcast()
}

/// Builds one group's review page.
fn build_review_page(dialog: &adw::Window, group: &ContactMergeGroup, initial_keeper: usize, on_merge: Rc<dyn Fn(usize, MergeChoices)>) -> gtk::Widget {
    let cards: Vec<&VCard> = group.entries.iter().map(|entry| entry.card()).collect();
    let fields = MergeFields::build(&cards);
    let defaults = default_choices(&cards, initial_keeper);
    let source_labels: Vec<String> = group.entries.iter().map(entry_source_label).collect();

    let form = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    let intro = gtk::Label::builder()
        .label("Choose the details to keep. The contact you keep is updated; the other copies are deleted after a successful merge.")
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    crate::privacy::mark_ui(&intro);
    form.append(&intro);

    let keeper_labels: Vec<&str> = source_labels.iter().map(String::as_str).collect();
    let keeper_dropdown = gtk::DropDown::from_strings(&keeper_labels);
    keeper_dropdown.set_selected(initial_keeper.min(source_labels.len().saturating_sub(1)) as u32);
    keeper_dropdown.set_tooltip_text(Some("The contact to keep and update"));
    keeper_dropdown.update_property(&[gtk::accessible::Property::Label("Contact to keep")]);
    crate::privacy::mark_content(&keeper_dropdown);

    let keeper_group = adw::PreferencesGroup::new();
    let keeper_row = adw::ActionRow::builder().title("Keep").subtitle("The other contacts are deleted after the merge.").build();
    keeper_row.add_suffix(&keeper_dropdown);
    keeper_group.add(&keeper_row);
    form.append(&keeper_group);

    let scalar_rows: Rc<RefCell<Vec<(MergeScalar, usize, gtk::CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));
    let list_rows: Rc<RefCell<Vec<(MergeList, String, gtk::CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));
    let note_candidates: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let mut combine_notes: Option<gtk::CheckButton> = None;

    for (field, candidates) in &fields.scalars {
        let section = adw::PreferencesGroup::builder().title(scalar_label(*field)).build();
        let default_source = defaults.scalars.get(field).copied().flatten();
        let mut first: Option<gtk::CheckButton> = None;
        let mut any_active = false;

        if *field == MergeScalar::Note && !candidates.is_empty() {
            let row = adw::ActionRow::builder()
                .title("Combine notes from all contacts")
                .subtitle("Join each contact's note into one, separated by a blank line.")
                .build();
            let check = gtk::CheckButton::new();
            check.set_active(true);
            check.set_halign(gtk::Align::Center);
            check.update_property(&[gtk::accessible::Property::Label("Combine notes from all contacts")]);
            row.add_suffix(&check);
            row.set_activatable(true);
            {
                let check = check.clone();
                row.connect_activated(move |_| check.set_active(!check.is_active()));
            }
            crate::privacy::mark_ui(&row);
            section.add(&row);
            combine_notes = Some(check);
        }

        for candidate in candidates {
            let check = gtk::CheckButton::new();
            check.set_halign(gtk::Align::Center);
            if let Some(first) = &first {
                check.set_group(Some(first));
            } else {
                first = Some(check.clone());
            }
            let active = default_source == Some(candidate.source) && !any_active;
            check.set_active(active);
            any_active |= active;
            let source = source_summary(&candidate.sources, &source_labels);
            check.update_property(&[gtk::accessible::Property::Label(&format!("Use {} {}", candidate.text, source))]);
            let row = adw::ActionRow::builder()
                .title(preview(&candidate.text, VALUE_PREVIEW_CHARS))
                .subtitle(source)
                .tooltip_text(&candidate.text)
                .build();
            row.add_suffix(&check);
            row.set_activatable(true);
            {
                let check = check.clone();
                row.connect_activated(move |_| check.set_active(true));
            }
            crate::privacy::mark_row(&row);
            section.add(&row);
            if *field == MergeScalar::Note {
                note_candidates.borrow_mut().push(row.clone());
            }
            scalar_rows.borrow_mut().push((*field, candidate.source, check));
        }
        if !any_active {
            if let Some(first) = first {
                first.set_active(true);
            }
        }
        if *field == MergeScalar::Note {
            if let Some(check) = &combine_notes {
                let note_candidates = note_candidates.clone();
                let sensitive = !check.is_active();
                for row in note_candidates.borrow().iter() {
                    row.set_sensitive(sensitive);
                }
                check.connect_toggled(move |check| {
                    let sensitive = !check.is_active();
                    for row in note_candidates.borrow().iter() {
                        row.set_sensitive(sensitive);
                    }
                });
            }
        }
        if !candidates.is_empty() {
            form.append(&section);
        }
    }

    for (field, candidates) in &fields.lists {
        let section = adw::PreferencesGroup::builder().title(list_label(*field)).build();
        for candidate in candidates {
            let check = gtk::CheckButton::new();
            check.set_halign(gtk::Align::Center);
            check.set_active(defaults.lists.get(field).is_some_and(|selected| selected.contains(&candidate.key)));
            let source = source_summary(&candidate.sources, &source_labels);
            check.update_property(&[gtk::accessible::Property::Label(&format!("Keep {} {}", candidate.text, source))]);
            let row = adw::ActionRow::builder()
                .title(preview(&candidate.text, VALUE_PREVIEW_CHARS))
                .subtitle(source)
                .tooltip_text(&candidate.text)
                .build();
            row.add_suffix(&check);
            row.set_activatable(true);
            {
                let check = check.clone();
                row.connect_activated(move |_| check.set_active(!check.is_active()));
            }
            crate::privacy::mark_row(&row);
            section.add(&row);
            list_rows.borrow_mut().push((*field, candidate.key.clone(), check));
        }
        if !candidates.is_empty() {
            form.append(&section);
        }
    }

    let cancel_button = gtk::Button::with_label("Cancel");
    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }
    let merge_button = gtk::Button::with_label("Merge");
    merge_button.add_css_class("suggested-action");
    {
        let keeper_dropdown = keeper_dropdown.clone();
        let scalar_rows = scalar_rows.clone();
        let list_rows = list_rows.clone();
        let list_fields: Vec<MergeList> = fields.lists.keys().copied().collect();
        let combine_notes = combine_notes.clone();
        let on_merge = on_merge.clone();
        merge_button.connect_clicked(move |_| {
            let keeper = keeper_dropdown.selected() as usize;
            let mut choices = MergeChoices::default();
            for (field, source, check) in scalar_rows.borrow().iter() {
                if check.is_active() {
                    choices.scalars.insert(*field, Some(*source));
                }
            }
            // Start every displayed list field with an empty selection: a
            // field that is left entirely unchecked is deliberately cleared,
            // not silently left at the keeper's value.
            for field in &list_fields {
                choices.lists.insert(*field, BTreeSet::new());
            }
            for (field, key, check) in list_rows.borrow().iter() {
                if check.is_active() {
                    choices.lists.entry(*field).or_default().insert(key.clone());
                }
            }
            choices.combine_notes = combine_notes.as_ref().is_some_and(|check| check.is_active());
            on_merge(keeper, choices);
        });
    }

    let button_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_top(6)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    button_row.append(&cancel_button);
    button_row.append(&merge_button);

    let scroller = gtk::ScrolledWindow::builder()
        .child(&form)
        .vexpand(true)
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root.append(&scroller);
    root.append(&button_row);
    root.upcast()
}

/// Validates the produced card, asks for confirmation, then applies it.
fn present_merge_confirmation(
    dialog: &adw::Window,
    state: &Rc<RefCell<UiState>>,
    toast_overlay: &adw::ToastOverlay,
    group: &ContactMergeGroup,
    keeper: usize,
    choices: MergeChoices,
) {
    let cards: Vec<&VCard> = group.entries.iter().map(|entry| entry.card()).collect();
    let Some(card) = merge_cards(&cards, keeper, &choices) else { return };
    if card.full_name.as_deref().map(str::trim).unwrap_or("").is_empty() && card.emails.is_empty() && card.telephones.is_empty() {
        show_error(dialog, "Nothing to merge", "Choose at least a name, email address or phone number.");
        return;
    }
    let Some(keeper_entry) = group.entries.get(keeper).cloned() else { return };
    let duplicates: Vec<ContactsListEntry> = group
        .entries
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != keeper)
        .map(|(_, entry)| entry.clone())
        .collect();
    if duplicates.is_empty() {
        return;
    }

    let heading = format!("Merge {} contacts?", group.entries.len());
    let body = format!(
        "{} will be kept. The other {} {} will be deleted. This can't be undone.",
        vcard_display_name(keeper_entry.card()),
        duplicates.len(),
        if duplicates.len() == 1 { "contact" } else { "contacts" }
    );
    let alert = adw::AlertDialog::builder()
        .heading(&heading)
        .body(&body)
        .default_response("cancel")
        .close_response("cancel")
        .build();
    alert.add_response("cancel", "Cancel");
    alert.add_response("merge", "Merge");
    alert.set_response_appearance("merge", adw::ResponseAppearance::Destructive);
    {
        let dialog = dialog.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        alert.connect_response(None, move |_, response| {
            if response == "merge" {
                apply_merge(&dialog, &state, &toast_overlay, keeper_entry.clone(), duplicates.clone(), card.clone());
            }
        });
    }
    alert.present(Some(dialog));
}

/// Updates the keeper first, and only deletes the duplicate copies once that
/// write succeeded. A failed update leaves every contact untouched; a failed
/// delete is reported while the successful merges and deletions stand.
fn apply_merge(dialog: &adw::Window, state: &Rc<RefCell<UiState>>, toast_overlay: &adw::ToastOverlay, keeper: ContactsListEntry, duplicates: Vec<ContactsListEntry>, card: VCard) {
    if keeper.href().is_empty() {
        show_error(dialog, "Nothing to merge", "This contact can't be updated here.");
        return;
    }
    // A keeper without a UID gains one in the merged card, so the favourite
    // key can legitimately change across the write. Carry both sides.
    let keeper_old_key = (keeper.account_id.clone(), contact_identity(keeper.card()));
    let keeper_new_key = (keeper.account_id.clone(), contact_identity(&card));
    // Keep the review page from accepting a second Merge or Cancel while the
    // keeper update and the deletions are in flight.
    dialog.set_sensitive(false);
    let Some(reply) = dispatch_contact_command(state, &keeper.account_id, {
        let href = keeper.href().to_string();
        let etag = keeper.etag().map(str::to_string);
        move |reply| ContactCommand::Update { href, etag, card, reply }
    }) else {
        dialog.set_sensitive(true);
        show_error(dialog, "Couldn't merge contacts", "The contacts session isn't running.");
        return;
    };
    let state = state.clone();
    let toast_overlay = toast_overlay.clone();
    let dialog = dialog.clone();
    glib::spawn_future_local(async move {
        match reply.recv().await {
            Ok(Ok(())) => {
                let mut deleted: Vec<ContactsListEntry> = Vec::new();
                let mut failures: Vec<String> = Vec::new();
                for duplicate in &duplicates {
                    let Some(delete_reply) = dispatch_contact_command(&state, &duplicate.account_id, {
                        let href = duplicate.href().to_string();
                        let etag = duplicate.etag().map(str::to_string);
                        move |reply| ContactCommand::Delete { href, etag, reply }
                    }) else {
                        failures.push("the contacts session isn't running".to_string());
                        continue;
                    };
                    match delete_reply.recv().await {
                        Ok(Ok(())) => deleted.push(duplicate.clone()),
                        Ok(Err(message)) => failures.push(message),
                        Err(_) => failures.push("the contacts session closed".to_string()),
                    }
                }
                transfer_favourites(&state, &keeper_old_key, &keeper_new_key, &deleted);
                let contacts_refresh = state.borrow().contacts_refresh.clone();
                if let Some(refresh) = contacts_refresh {
                    refresh();
                }
                let merged = deleted.len() + 1;
                let summary = if failures.is_empty() {
                    format!("Merged {merged} contact{}", if merged == 1 { "" } else { "s" })
                } else {
                    format!(
                        "Merged into {}; {} duplicate{} couldn't be deleted",
                        vcard_display_name(keeper.card()),
                        failures.len(),
                        if failures.len() == 1 { "" } else { "s" }
                    )
                };
                let summary = glib::markup_escape_text(&summary);
                toast_overlay.add_toast(adw::Toast::new(&summary));
                dialog.close();
            }
            Ok(Err(message)) => {
                dialog.set_sensitive(true);
                show_error(&dialog, "Couldn't merge contacts", &message);
            }
            // The session died mid-write; its own error handling owns a toast.
            Err(_) => {
                dialog.set_sensitive(true);
                show_error(&dialog, "Couldn't merge contacts", "The contacts session closed before the update finished.");
            }
        }
    });
}

/// Carries a favourite from the keeper's old identity or any deleted copy to
/// the keeper's post-merge identity, then removes the stale identities from
/// the local list and its database.
fn transfer_favourites(state: &Rc<RefCell<UiState>>, keeper_old_key: &(AccountId, String), keeper_new_key: &(AccountId, String), deleted: &[ContactsListEntry]) {
    let deleted_keys: Vec<(AccountId, String)> = deleted.iter().map(|entry| (entry.account_id.clone(), contact_identity(entry.card()))).collect();
    let (was_starred, any_deleted_starred, db) = {
        let st = state.borrow();
        (
            st.starred_contacts.contains(keeper_old_key) || st.starred_contacts.contains(keeper_new_key),
            deleted_keys.iter().any(|key| st.starred_contacts.contains(key)),
            st.ui_db.clone(),
        )
    };
    let star_new = was_starred || any_deleted_starred;
    {
        let mut st = state.borrow_mut();
        if star_new {
            st.starred_contacts.insert(keeper_new_key.clone());
        }
        if keeper_old_key != keeper_new_key {
            st.starred_contacts.remove(keeper_old_key);
        }
        for key in &deleted_keys {
            // A UID-less card can share its email-based identity with the
            // keeper; never remove the star that was just carried over.
            if key != keeper_new_key {
                st.starred_contacts.remove(key);
            }
        }
    }
    if let Some(db) = db {
        if star_new {
            let _ = db.set_starred(&keeper_new_key.0, &keeper_new_key.1, true);
        }
        if keeper_old_key != keeper_new_key {
            let _ = db.set_starred(&keeper_old_key.0, &keeper_old_key.1, false);
        }
        for key in &deleted_keys {
            if key != keeper_new_key {
                let _ = db.set_starred(&key.0, &key.1, false);
            }
        }
    }
}

/// A small error alert parented to the merge dialog, so a failure is visible
/// above the modal window instead of behind it in the main window's toast
/// overlay.
fn show_error(dialog: &adw::Window, heading: &str, message: &str) {
    let alert = adw::AlertDialog::builder()
        .heading(heading)
        .body(message)
        .default_response("close")
        .close_response("close")
        .build();
    alert.add_response("close", "Close");
    alert.present(Some(dialog));
}

fn entry_source_label(entry: &ContactsListEntry) -> String {
    let name = vcard_display_name(entry.card());
    let email = vcard_primary_email(entry.card());
    if email == "No email address" {
        name
    } else {
        format!("{name} · {email}")
    }
}

fn source_summary(sources: &[usize], labels: &[String]) -> String {
    let Some(first) = sources.first().and_then(|index| labels.get(*index)) else {
        return String::new();
    };
    if sources.len() > 1 {
        format!("from {first} +{}", sources.len() - 1)
    } else {
        format!("from {first}")
    }
}

fn preview(text: &str, limit: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= limit {
        flattened
    } else {
        format!("{}…", flattened.chars().take(limit).collect::<String>())
    }
}

fn scalar_label(field: MergeScalar) -> &'static str {
    match field {
        MergeScalar::Name => "Full name",
        MergeScalar::Organization => "Organization",
        MergeScalar::Title => "Job title",
        MergeScalar::FileAs => "File as",
        MergeScalar::Birthday => "Birthday",
        MergeScalar::Anniversary => "Anniversary",
        MergeScalar::Note => "Notes",
        MergeScalar::Photo => "Photo",
    }
}

fn list_label(field: MergeList) -> &'static str {
    match field {
        MergeList::Email => "Email",
        MergeList::Phone => "Phone",
        MergeList::Address => "Address",
        MergeList::Url => "Website",
        MergeList::Impp => "Instant messaging",
        MergeList::Category => "Categories",
        MergeList::Custom => "Custom fields",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::VCard;

    fn record(href: &str, card: VCard) -> lookout_dav::ContactRecord {
        lookout_dav::ContactRecord {
            href: href.to_string(),
            etag: Some("etag".to_string()),
            card,
        }
    }

    fn person(uid: &str, email: &str) -> VCard {
        VCard::parse(&format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{uid}\r\nEMAIL:{email}\r\nEND:VCARD\r\n")).unwrap()
    }

    #[test]
    fn scan_groups_matches_within_one_account_only() {
        let mut accounts = HashMap::new();
        accounts.insert(
            AccountId("work".to_string()),
            crate::contacts_view::test_snapshot(
                "Work",
                vec![record("/a/one.vcf", person("a", "ada@example.com")), record("/a/two.vcf", person("b", "ada@example.com"))],
            ),
        );
        accounts.insert(
            AccountId("home".to_string()),
            crate::contacts_view::test_snapshot("Home", vec![record("/b/one.vcf", person("c", "ada@example.com"))]),
        );
        let groups = scan_groups(&accounts);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].account_label, "Work");
        assert_eq!(groups[0].entries.len(), 2);
    }

    #[test]
    fn scan_groups_excludes_lists_and_unwritable_records() {
        let list = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:group\r\nUID:list\r\nFN:Crew\r\nEMAIL:ada@example.com\r\nEND:VCARD\r\n").unwrap();
        let ada = person("a", "ada@example.com");
        let mut accounts = HashMap::new();
        accounts.insert(
            AccountId("work".to_string()),
            crate::contacts_view::test_snapshot(
                "Work",
                vec![record("/a/person.vcf", ada), record("/a/list.vcf", list), record("", person("readonly", "ada@example.com"))],
            ),
        );
        assert!(scan_groups(&accounts).is_empty());
    }

    #[test]
    fn scan_groups_puts_the_most_complete_card_first() {
        let sparse = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:sparse\r\nFN:Ada\r\nEMAIL:ada@example.com\r\nEND:VCARD\r\n").unwrap();
        let full = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:full\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.com\r\nTEL:+15551234\r\nNOTE:Hi\r\nEND:VCARD\r\n").unwrap();
        let mut accounts = HashMap::new();
        accounts.insert(
            AccountId("work".to_string()),
            crate::contacts_view::test_snapshot("Work", vec![record("/a/sparse.vcf", sparse), record("/a/full.vcf", full)]),
        );
        let groups = scan_groups(&accounts);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].entries[0].card().uid.as_deref(), Some("full"));
    }
}
