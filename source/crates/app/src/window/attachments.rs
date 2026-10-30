//! Attachment strips, fetch/save/zip flows and temp-file open helpers.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::text::human_size;
use lookout_core::{BodyPart, EmailSummary, MailboxId, Uid};
use lookout_mail::session::AccountCommand;
use lookout_mail::Attachment;

use crate::message_list::MessageListModel;
use crate::worker::Worker;

use super::*;

/// A safe save-suggested name for an attachment without one: falls back to a
/// part-number-based placeholder (the raw wire could be anything, and a
/// filename containing `/` from a hostile header must not be fed to the save
/// dialog as a path). `filename` is trusted when present - it's just the
/// initial name in the dialog, which the user can change.
pub(crate) fn attachment_display_name(part: &BodyPart) -> String {
    part.filename
        .as_deref()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty() && !f.contains('/') && !f.contains('\\'))
        .map(str::to_string)
        .unwrap_or_else(|| format!("attachment-{}", part.part_number))
}

/// Rebuilds the reading pane's attachment strip for the message about to be
/// rendered: clears whatever's there, then one row per attachment `BodyPart`
/// (`is_attachment` only - inline `cid:` images stay in the HTML body). Each
/// row shows the attachment's name and size plus a menu button whose popover
/// offers **Open** (the MIME type's default handler, via a temporary file
/// deleted when Lookout exits), **Open With…** (a chooser dialog for the
/// handler), and **Save…** (a save-location dialog) - all three ask the
/// account session for that part's bytes on demand (`FetchAttachment`). The
/// strip is hidden when the message has no attachments. One action is in
/// flight at a time, tracked as the single `UiState::pending_attachment`.
pub(crate) fn rebuild_attachment_strip(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, mailbox: &MailboxId, uid: Uid, parts: &[BodyPart]) {
    let Some(strip) = find_attachment_strip(reading_stack) else { return };
    // A previous render's in-flight action belongs to the message being
    // replaced; any late `PartFetched` for it is discarded as stale.
    state.borrow_mut().pending_attachment = None;
    while let Some(child) = strip.first_child() {
        strip.remove(&child);
    }

    let attachments: Vec<&BodyPart> = parts.iter().filter(|p| p.is_attachment).collect();
    strip.set_visible(!attachments.is_empty());
    // "Save all" / "Download as zip" (item 1.23 / M-33): a header row above
    // the individual attachments, shown once there's more than one to gather.
    if attachments.len() >= 2 {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("attachment-row");
        let label = gtk::Label::new(Some(&format!("{} attachments", attachments.len())));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        let menu_button = gtk::MenuButton::new();
        menu_button.set_label("Save all");
        menu_button.add_css_class("flat");
        menu_button.set_tooltip_text(Some("Save or download every attachment"));
        let popover = gtk::Popover::new();
        let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
        items.set_margin_top(6);
        items.set_margin_bottom(6);
        items.set_margin_start(6);
        items.set_margin_end(6);
        let save_all_item = gtk::Button::with_label("Save all…");
        save_all_item.add_css_class("flat");
        save_all_item.set_halign(gtk::Align::Start);
        let zip_item = gtk::Button::with_label("Download as zip…");
        zip_item.add_css_class("flat");
        zip_item.set_halign(gtk::Align::Start);
        items.append(&save_all_item);
        items.append(&zip_item);
        popover.set_child(Some(&items));
        menu_button.set_popover(Some(&popover));
        header.append(&label);
        header.append(&menu_button);
        let batch_parts: Vec<BodyPart> = attachments.iter().map(|p| (*p).clone()).collect();
        {
            let state = state.clone();
            let mailbox = mailbox.clone();
            let parts = batch_parts.clone();
            save_all_item.connect_clicked(move |button| {
                let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
                let state = state.clone();
                let mailbox = mailbox.clone();
                let parts = parts.clone();
                glib::spawn_future_local(async move {
                    let dialog = gtk::FileDialog::builder().title("Save all attachments").build();
                    let Ok(folder) = dialog.select_folder_future(Some(&window)).await else { return };
                    start_attachment_batch(&state, &mailbox, uid, parts, AttachmentBatchAction::SaveAll(folder));
                });
            });
        }
        {
            let state = state.clone();
            let mailbox = mailbox.clone();
            let parts = batch_parts;
            zip_item.connect_clicked(move |button| {
                let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
                let state = state.clone();
                let mailbox = mailbox.clone();
                let parts = parts.clone();
                glib::spawn_future_local(async move {
                    let dialog = gtk::FileDialog::builder().title("Download attachments").initial_name("attachments.zip").build();
                    let Ok(file) = dialog.save_future(Some(&window)).await else { return };
                    start_attachment_batch(&state, &mailbox, uid, parts, AttachmentBatchAction::Zip(file));
                });
            });
        }
        strip.append(&header);
    }
    for part in attachments {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("attachment-row");
        let icon = gtk::Image::from_icon_name("mail-attachment-symbolic");
        icon.add_css_class("dim-label");
        let name = attachment_display_name(part);
        let label = gtk::Label::new(Some(&format!("{name}  ·  {}", human_size(part.size.into()))));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        let menu_button = gtk::MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.add_css_class("flat");
        menu_button.set_tooltip_text(Some("Save or open attachment"));
        let popover = gtk::Popover::new();
        let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
        items.set_margin_top(6);
        items.set_margin_bottom(6);
        items.set_margin_start(6);
        items.set_margin_end(6);
        let open_item = gtk::Button::with_label("Open");
        open_item.add_css_class("flat");
        open_item.set_halign(gtk::Align::Start);
        let open_with_item = gtk::Button::with_label("Open With…");
        open_with_item.add_css_class("flat");
        open_with_item.set_halign(gtk::Align::Start);
        let save_item = gtk::Button::with_label("Save…");
        save_item.add_css_class("flat");
        save_item.set_halign(gtk::Align::Start);
        items.append(&open_item);
        items.append(&open_with_item);
        items.append(&save_item);
        popover.set_child(Some(&items));
        menu_button.set_popover(Some(&popover));
        row.append(&icon);
        row.append(&label);
        row.append(&menu_button);

        let button_for_open = menu_button.clone();
        let state_for_open = state.clone();
        let mailbox_for_open = mailbox.clone();
        let part_for_open = part.clone();
        open_item.connect_clicked(move |_| {
            start_attachment_fetch(&state_for_open, &mailbox_for_open, uid, &part_for_open, &button_for_open, PendingAttachmentAction::Open);
        });
        // Double-clicking the row is the same as choosing "Open".
        let gesture_open = gtk::GestureClick::new();
        gesture_open.set_button(gtk::gdk::BUTTON_PRIMARY);
        let button_for_dclick = menu_button.clone();
        let state_for_dclick = state.clone();
        let mailbox_for_dclick = mailbox.clone();
        let part_for_dclick = part.clone();
        gesture_open.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                start_attachment_fetch(
                    &state_for_dclick,
                    &mailbox_for_dclick,
                    uid,
                    &part_for_dclick,
                    &button_for_dclick,
                    PendingAttachmentAction::Open,
                );
            }
        });
        row.add_controller(gesture_open);
        let button_for_open_with = menu_button.clone();
        let state_for_open_with = state.clone();
        let mailbox_for_open_with = mailbox.clone();
        let part_for_open_with = part.clone();
        open_with_item.connect_clicked(move |_| {
            start_attachment_fetch(
                &state_for_open_with,
                &mailbox_for_open_with,
                uid,
                &part_for_open_with,
                &button_for_open_with,
                PendingAttachmentAction::OpenWith,
            );
        });
        let button_for_save = menu_button.clone();
        let state_for_save = state.clone();
        let mailbox_for_save = mailbox.clone();
        let part_for_save = part.clone();
        save_item.connect_clicked(move |_| {
            start_attachment_fetch(&state_for_save, &mailbox_for_save, uid, &part_for_save, &button_for_save, PendingAttachmentAction::Save);
        });

        strip.append(&row);
    }
}

/// Starts an attachment action (Save or Open) for one strip row: records it
/// as the single in-flight `UiState::pending_attachment`, disables the row's
/// menu button, and asks the account session for the part's bytes
/// (`AccountCommand::FetchAttachment`). Also arms the 60-second backstop that
/// restores the button if no answer ever arrives (the session dying mid-fetch
/// loses the command to the reconnect) - a resolved or superseded request is
/// left alone.
pub(crate) fn start_attachment_fetch(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, part: &BodyPart, button: &gtk::MenuButton, action: PendingAttachmentAction) {
    // One in-flight fetch at a time; ignore a click while one (or a
    // save-all/zip batch) is outstanding.
    {
        let st = state.borrow();
        if st.pending_attachment.is_some() || st.pending_attachment_batch.is_some() {
            return;
        }
    }
    let cmd_tx = match mailbox_account_id(mailbox) {
        Some(id) => {
            let st = state.borrow();
            st.accounts.get(&id).map(|h| h.interactive_cmd_tx.clone())
        }
        None => None,
    };
    let Some(cmd_tx) = cmd_tx else { return };
    state.borrow_mut().pending_attachment = Some(PendingAttachment {
        mailbox: mailbox.clone(),
        uid,
        part_number: part.part_number.clone(),
        action,
        button: button.clone(),
    });
    button.set_sensitive(false);
    tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, action = ?action, "attachment action: dispatching to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchAttachment {
        mailbox: mailbox.clone(),
        uid,
        part: part.clone(),
    });
    // Backstop: restore the button after a generous grace period instead of
    // leaving it stuck. Only fires if this exact request is still the
    // outstanding one.
    let timeout_mailbox = mailbox.clone();
    let timeout_part = part.part_number.clone();
    let timeout_button = button.clone();
    let timeout_state = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let still_pending = match &timeout_state.borrow().pending_attachment {
            Some(p) => p.mailbox == timeout_mailbox && p.uid == uid && p.part_number == timeout_part,
            None => false,
        };
        if still_pending {
            let mut st = timeout_state.borrow_mut();
            st.pending_attachment = None;
            drop(st);
            timeout_button.set_sensitive(true);
            if let Some(toast_overlay) = &timeout_state.borrow().toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Attachment fetch timed out"));
            }
        }
        glib::ControlFlow::Break
    });
}

/// Picks a save name for `part` that doesn't collide with a name already in
/// `used`: the part's display name, or "name (2).ext" style when it does.
pub(crate) fn unique_attachment_name(part: &BodyPart, used: &mut HashSet<String>) -> String {
    let base = attachment_display_name(part);
    if used.insert(base.clone()) {
        return base;
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (base.clone(), String::new()),
    };
    let mut n = 2;
    loop {
        let candidate = format!("{stem} ({n}){ext}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Starts a "Save all" / "Download as zip" run (item 1.23 / M-33): records the
/// batch, then asks the session for the first attachment's bytes. The rest
/// follow as each `PartFetched` lands (`advance_attachment_batch`), and the
/// whole set is written once the queue drains (`finish_attachment_batch`).
pub(crate) fn start_attachment_batch(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, parts: Vec<BodyPart>, action: AttachmentBatchAction) {
    if parts.is_empty() {
        return;
    }
    // One batch - or single-row fetch - at a time.
    {
        let st = state.borrow();
        if st.pending_attachment_batch.is_some() || st.pending_attachment.is_some() {
            return;
        }
    }
    let cmd_tx = mailbox_account_id(mailbox).and_then(|id| state.borrow().accounts.get(&id).map(|h| h.interactive_cmd_tx.clone()));
    let Some(cmd_tx) = cmd_tx else { return };
    let mut queue: VecDeque<BodyPart> = parts.into();
    let first = queue.pop_front().expect("parts is non-empty");
    let current = first.part_number.clone();
    state.borrow_mut().pending_attachment_batch = Some(AttachmentBatch {
        mailbox: mailbox.clone(),
        uid,
        current,
        queue,
        collected: Vec::new(),
        action,
    });
    let _ = cmd_tx.send_blocking(AccountCommand::FetchAttachment {
        mailbox: mailbox.clone(),
        uid,
        part: first,
    });
    arm_attachment_batch_timeout(state, mailbox, uid);
}

/// The 60-second backstop for a batch: a session that dies mid-run would
/// otherwise leave the batch stuck (blocking later runs). Only fires if this
/// exact batch is still outstanding.
pub(crate) fn arm_attachment_batch_timeout(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid) {
    let timeout_state = state.clone();
    let timeout_mailbox = mailbox.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let timed_out = {
            let mut st = timeout_state.borrow_mut();
            let matches_batch = st.pending_attachment_batch.as_ref().is_some_and(|b| b.mailbox == timeout_mailbox && b.uid == uid);
            if matches_batch {
                let is_forward = matches!(st.pending_attachment_batch.as_ref().map(|b| &b.action), Some(AttachmentBatchAction::Forward(_)));
                st.pending_attachment_batch = None;
                Some(is_forward)
            } else {
                None
            }
        };
        if let Some(is_forward) = timed_out {
            if let Some(toast_overlay) = &timeout_state.borrow().toast_overlay {
                let message = if is_forward {
                    "Forward timed out - the original's files couldn't be attached"
                } else {
                    "Attachment download timed out"
                };
                toast_overlay.add_toast(adw::Toast::new(message));
            }
        }
        glib::ControlFlow::Break
    });
}

/// Feeds one fetched part into the in-flight batch: stores it under a
/// de-duplicated name, requests the next part, or finishes the run once the
/// queue drains. Returns `true` when the part belonged to the batch, so the
/// single-row path is skipped.
pub(crate) fn advance_attachment_batch(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    mailbox: &MailboxId,
    uid: Uid,
    part: &BodyPart,
    bytes: &[u8],
) -> bool {
    let next = {
        let mut st = state.borrow_mut();
        let Some(batch) = st.pending_attachment_batch.as_mut() else {
            return false;
        };
        if batch.mailbox != *mailbox || batch.uid != uid || batch.current != part.part_number {
            return false;
        }
        let mut used: HashSet<String> = batch.collected.iter().map(|c| c.name.clone()).collect();
        let name = unique_attachment_name(part, &mut used);
        batch.collected.push(CollectedAttachment {
            part: part.clone(),
            name,
            bytes: bytes.to_vec(),
        });
        let next = batch.queue.pop_front();
        if let Some(next_part) = &next {
            batch.current = next_part.part_number.clone();
        }
        next
    };
    match next {
        Some(next_part) => {
            let cmd_tx = mailbox_account_id(mailbox).and_then(|id| state.borrow().accounts.get(&id).map(|h| h.interactive_cmd_tx.clone()));
            if let Some(cmd_tx) = cmd_tx {
                let _ = cmd_tx.send_blocking(AccountCommand::FetchAttachment {
                    mailbox: mailbox.clone(),
                    uid,
                    part: next_part,
                });
            }
        }
        None => finish_attachment_batch(state, worker, reading_stack),
    }
    true
}

/// Finishes a drained batch: writes its attachments to the chosen folder or a
/// zip ("Save all" / "Download as zip", item 1.23), or opens a forward
/// composer with them (item 1.11 / C-13).
pub(crate) fn finish_attachment_batch(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack) {
    let Some(batch) = state.borrow_mut().pending_attachment_batch.take() else {
        return;
    };
    let toast_overlay = state.borrow().toast_overlay.clone();
    let AttachmentBatch { action, collected, .. } = batch;
    match action {
        AttachmentBatchAction::Forward(fb) => open_forward_composer(state, worker, reading_stack, *fb, collected),
        AttachmentBatchAction::SaveAll(folder) => {
            glib::spawn_future_local(async move {
                let files: Vec<(String, Vec<u8>)> = collected.into_iter().map(|c| (c.name, c.bytes)).collect();
                let result = save_batch_to_folder(&folder, &files).await;
                if let Some(toast_overlay) = toast_overlay {
                    match result {
                        Ok(message) => toast_overlay.add_toast(adw::Toast::new(&message)),
                        Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't save attachments: {e}")))),
                    }
                }
            });
        }
        AttachmentBatchAction::Zip(file) => {
            glib::spawn_future_local(async move {
                let files: Vec<(String, Vec<u8>)> = collected.into_iter().map(|c| (c.name, c.bytes)).collect();
                let result = save_batch_as_zip(&file, &files).await;
                if let Some(toast_overlay) = toast_overlay {
                    match result {
                        Ok(message) => toast_overlay.add_toast(adw::Toast::new(&message)),
                        Err(e) => toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't save attachments: {e}")))),
                    }
                }
            });
        }
    }
}

/// A part's `type/subtype`, or `application/octet-stream` when the wire
/// carried none - the same default the attachment strip's save path uses.
pub(crate) fn part_content_type(part: &BodyPart) -> String {
    let content_type = part.content_type.trim();
    if content_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        content_type.to_string()
    }
}

/// Opens a forward composer once its batch has drained (item 1.11 / C-13):
/// the fetched `is_attachment` parts become the composer's attachments and the
/// fetched inline (`cid:`) parts resolve the forwarded HTML's image
/// references to `data:` URIs, so a rich original forwards with its formatting
/// and pictures intact. A part that failed to fetch simply doesn't appear -
/// the composer still opens with whatever arrived (§10, degrade softly).
pub(crate) fn open_forward_composer(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack, fb: ForwardBatch, collected: Vec<CollectedAttachment>) {
    let mut prefill = fb.prefill;
    let mut attachments: Vec<Attachment> = Vec::new();
    let mut images: Vec<(String, String, Vec<u8>)> = Vec::new();
    for c in collected {
        if c.part.is_attachment {
            attachments.push(Attachment {
                filename: c.name,
                content_type: part_content_type(&c.part),
                bytes: c.bytes.into(),
            });
        } else if let Some(cid) = c.part.cid.clone() {
            images.push((cid, part_content_type(&c.part), c.bytes));
        }
    }
    prefill.attachments = attachments;
    if let Some(html) = prefill.html_body.take() {
        prefill.html_body = Some(lookout_mail::body::rewrite_cid_refs_with(&html, &images));
    }
    let rich_text_default = state.borrow().rich_text_default;
    show_composer_in_reading_pane_full(
        state,
        worker,
        reading_stack,
        &fb.title,
        fb.cmd_tx,
        prefill,
        rich_text_default,
        fb.account_id,
        fb.signature_kind,
        None,
        fb.pop_out_on_open,
    );
}

/// Writes each `(name, bytes)` into `folder` (item 1.23's "Save all").
pub(crate) async fn save_batch_to_folder(folder: &gio::File, collected: &[(String, Vec<u8>)]) -> Result<String, String> {
    for (name, bytes) in collected {
        folder
            .child(name.as_str())
            .replace_contents_bytes_future(&glib::Bytes::from(bytes.as_slice()), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
            .await
            .map_err(|e| e.to_string())?;
    }
    let noun = if collected.len() == 1 { "attachment" } else { "attachments" };
    Ok(format!("Saved {} {noun}", collected.len()))
}

/// Builds a zip archive of `collected` in memory and writes it to `file`
/// (item 1.23's "Download as zip").
pub(crate) async fn save_batch_as_zip(file: &gio::File, collected: &[(String, Vec<u8>)]) -> Result<String, String> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in collected {
            writer.start_file(name.clone(), options).map_err(|e| e.to_string())?;
            writer.write_all(bytes).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }
    let data = cursor.into_inner();
    file.replace_contents_bytes_future(&glib::Bytes::from(data.as_slice()), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("Saved {} attachments as a zip", collected.len()))
}

/// Starts the .eml export for the currently-selected message: records it as
/// the single in-flight `UiState::pending_raw_message`, asks the account
/// session for the whole raw message (`AccountCommand::FetchRawMessage`), and
/// arms the same 60-second backstop as attachment fetches. Silent no-op when
/// nothing is selected or the account has disconnected (same convention as
/// the Reply handlers); the save dialog opens once the bytes arrive
/// (`AccountEvent::RawMessageFetched`).
pub(crate) fn start_raw_message_export(message_list: &MessageListModel, state: &Rc<RefCell<UiState>>) {
    let Some(summary) = message_list.selected_summary() else { return };
    let Some(account_id) = mailbox_account_id(&summary.mailbox) else { return };
    let cmd_tx = {
        let st = state.borrow();
        st.accounts.get(&account_id).map(|h| h.interactive_cmd_tx.clone())
    };
    let Some(cmd_tx) = cmd_tx else { return };
    // One export in flight at a time; ignore a click while one is outstanding.
    if state.borrow().pending_raw_message.is_some() {
        return;
    }
    state.borrow_mut().pending_raw_message = Some(PendingRawMessage {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
        initial_name: eml_suggested_name(&summary),
    });
    tracing::debug!(?summary.mailbox, uid = summary.uid.0, "raw message export: dispatching to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchRawMessage {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
    });
    // Backstop: clear the pending state after a generous grace period instead
    // of blocking future exports forever (the session dying mid-fetch loses
    // the command to the reconnect). Only fires if this exact request is
    // still the outstanding one.
    let timeout_mailbox = summary.mailbox.clone();
    let timeout_uid = summary.uid;
    let timeout_state = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let still_pending = match &timeout_state.borrow().pending_raw_message {
            Some(p) => p.mailbox == timeout_mailbox && p.uid == timeout_uid,
            None => false,
        };
        if still_pending {
            timeout_state.borrow_mut().pending_raw_message = None;
            if let Some(toast_overlay) = &timeout_state.borrow().toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Message export timed out"));
            }
        }
        glib::ControlFlow::Break
    });
}

/// Prompts for a save location (via the platform save dialog - a GTK
/// `FileDialog`, which goes through the XDG portal in sandboxed runs) and
/// writes the fetched attachment bytes there. The dialog is cancellable by
/// design; a write failure surfaces as a toast.
pub(crate) async fn save_attachment_to_disk(window: &adw::ApplicationWindow, toast_overlay: adw::ToastOverlay, part: &BodyPart, bytes: &[u8]) {
    let dialog = gtk::FileDialog::builder().title("Save attachment").initial_name(attachment_display_name(part)).build();
    let Ok(file) = dialog.save_future(Some(window)).await else { return };
    let result = file
        .replace_contents_bytes_future(&glib::Bytes::from(bytes), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
        .await;
    match result {
        Ok(_) => toast_overlay.add_toast(adw::Toast::new("Attachment saved")),
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't save attachment: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
        }
    }
}

/// Prompts for a save location and writes a whole raw message there as a
/// `.eml` file - `bytes` are the server's `BODY.PEEK[]` response verbatim, so
/// the file is a valid RFC 5322 message (and the session's raw-message cache
/// makes the export instant on re-save). Mirrors `save_attachment_to_disk`,
/// including its cancel-on-dialog-close behavior and toast feedback.
pub(crate) async fn save_raw_message_to_disk(window: &adw::ApplicationWindow, toast_overlay: adw::ToastOverlay, initial_name: &str, bytes: &[u8]) {
    let filter = gtk::FileFilter::new();
    filter.add_suffix("eml");
    filter.set_name(Some("Email messages (*.eml)"));
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title("Save message as .eml")
        .initial_name(initial_name)
        .filters(&filters)
        .build();
    let Ok(file) = dialog.save_future(Some(window)).await else { return };
    let result = file
        .replace_contents_bytes_future(&glib::Bytes::from(bytes), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
        .await;
    match result {
        Ok(_) => toast_overlay.add_toast(adw::Toast::new("Message saved as .eml")),
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't save message: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
        }
    }
}

/// A safe suggested filename for the .eml export dialog: the message's
/// subject plus an `.eml` suffix, or a plain placeholder when the subject is
/// empty or contains path separators - the raw subject is untrusted header
/// data and must not be fed to the dialog as a path (same rationale as
/// `attachment_display_name`). Just the initial name; the user can change it
/// in the dialog.
pub(crate) fn eml_suggested_name(summary: &EmailSummary) -> String {
    let subject = summary.subject.as_deref().unwrap_or("").trim();
    if subject.is_empty() || subject.contains('/') || subject.contains('\\') {
        "message.eml".to_string()
    } else {
        format!("{subject}.eml")
    }
}

/// The most of a raw message the details dialog's Source page will display.
/// A message with a multi-megabyte base64 attachment would otherwise hand
/// `GtkTextView` an enormous document and stall the UI; truncation is
/// announced inline, and the full bytes remain available through
/// "Save as .eml…".
pub(crate) const MESSAGE_SOURCE_DISPLAY_CAP: usize = 4 * 1024 * 1024;

/// A safe file extension for an attachment's temporary copy, so the system's
/// default handler can recognize the file type: the filename's own extension
/// when it's a clean alphanumeric token (≤ 8 chars), else a content-type map,
/// else a `bin` fallback (or the subtype when it's a single clean token, e.g.
/// `application/vnd.foo` → `vnd` is rejected for containing `-`, so it falls
/// through to `bin`).
pub(crate) fn attachment_extension(part: &BodyPart) -> String {
    let from_filename = part
        .filename
        .as_deref()
        .and_then(|f| std::path::Path::new(f).extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|ext| !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    if let Some(ext) = from_filename {
        return ext;
    }
    let subtype = part.content_type.split('/').nth(1).unwrap_or("");
    match part.content_type.as_str() {
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.oasis.opendocument.text" => "odt",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        "application/json" => "json",
        "application/xml" => "xml",
        "application/octet-stream" => "bin",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "message/rfc822" => "eml",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        // Unknown type: the subtype alone when it's a single clean token
        // ("octet-stream" contains a hyphen and correctly lands on `bin`).
        _ if !subtype.is_empty() && subtype.len() <= 8 && subtype.chars().all(|c| c.is_ascii_alphanumeric()) => subtype,
        _ => "bin",
    }
    .to_string()
}

/// The unique temp-file path an "Open" action materializes an attachment at:
/// `$TMPDIR/lookout-<uuid>.<ext>`, where the extension comes from
/// `attachment_extension` so the system's default handler recognizes the
/// type. A fresh uuid per call means concurrent opens never collide.
pub(crate) fn temp_attachment_path(part: &BodyPart) -> PathBuf {
    std::env::temp_dir().join(format!("lookout-{}.{}", uuid::Uuid::new_v4().simple(), attachment_extension(part)))
}

/// Writes the fetched attachment bytes to a unique temporary file and
/// registers it for deletion when Lookout exits. The caller keeps ownership
/// of the path for its own launch step; on a launch failure it should call
/// `discard_temp_attachment` so the file doesn't linger until exit.
pub(crate) fn materialize_temp_attachment(state: &Rc<RefCell<UiState>>, part: &BodyPart, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = temp_attachment_path(part);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    state.borrow_mut().temp_attachment_files.insert(path.clone());
    Ok(path)
}

/// Unregisters and deletes a temp file that will never be opened - a launch
/// failure or an "Open With" dialog dismissed without a choice.
pub(crate) fn discard_temp_attachment(state: &Rc<RefCell<UiState>>, path: &PathBuf) {
    state.borrow_mut().temp_attachment_files.remove(path);
    let _ = std::fs::remove_file(path);
}

/// Opens the fetched attachment bytes with the MIME type's *default*
/// application, from a temporary file. The default app is resolved directly
/// through GIO (`AppInfo::default_for_type`) and launched with
/// `AppInfo::launch` - a plain activation, no chooser, no portal - so a file
/// whose type has a registered default opens in exactly that app. Only when
/// there is no default app for the type (or the direct launch fails, e.g.
/// running sandboxed, where host apps can't be spawned from inside the
/// sandbox) does it fall back to `GtkFileLauncher`, which routes through the
/// XDG portal and lets the portal decide. The temp file is registered in
/// `UiState::temp_attachment_files` and deleted when Lookout exits; a write
/// or launch failure toasts and cleans up immediately.
pub(crate) async fn open_attachment_temp(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: adw::ToastOverlay, part: &BodyPart, bytes: &[u8]) {
    let path = match materialize_temp_attachment(state, part, bytes) {
        Ok(path) => path,
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't open attachment: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };
    let file = gio::File::for_path(&path);
    // The default application for the part's MIME type, if one is registered.
    let default_app = gio::content_type_from_mime_type(&part.content_type).and_then(|ct| gio::AppInfo::default_for_type(&ct, false));
    if let Some(app) = default_app {
        match app.launch(std::slice::from_ref(&file), None::<&gio::AppLaunchContext>) {
            Ok(()) => return,
            Err(e) => tracing::warn!("default app {:?} failed to launch attachment: {e}", app.name()),
        }
    }
    // No default app, or it couldn't be launched directly: let the portal
    // take it (it launches the default app, or asks the user when there is
    // none).
    let launcher = gtk::FileLauncher::new(Some(&file));
    if let Err(e) = launcher.launch_future(Some(window)).await {
        discard_temp_attachment(state, &path);
        let title = glib::markup_escape_text(&format!("Couldn't open attachment: {e}"));
        toast_overlay.add_toast(adw::Toast::new(&title));
    }
}

/// Opens the fetched attachment bytes with a user-chosen application, from a
/// temporary file. Asks through the XDG desktop portal's
/// `org.freedesktop.portal.OpenURI` with the `ask` option set - the portal
/// presents its own "Open With" application chooser (the same dialog the
/// portal shows when no default app exists) and launches the picked app on
/// the host, which is what makes this work from inside a sandbox. The temp
/// file is registered in `UiState::temp_attachment_files` and deleted when
/// Lookout exits; a write failure or a portal that isn't reachable toasts and
/// cleans up immediately. The portal's own response (cancelled vs. launched)
/// is deliberately not tracked - the file is simply kept until exit either
/// way, matching the rest of the Open actions.
pub(crate) async fn open_attachment_with(state: &Rc<RefCell<UiState>>, toast_overlay: adw::ToastOverlay, part: &BodyPart, bytes: &[u8]) {
    let path = match materialize_temp_attachment(state, part, bytes) {
        Ok(path) => path,
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't open attachment: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };

    // The portal's OpenURI method explicitly does not accept `file://` URIs -
    // local files go through OpenFile, which takes the file descriptor. Pass
    // the temp file's fd over the bus with the `ask` option set, so the
    // portal shows its own application chooser and launches the picked app
    // on the host (which is what makes this work from inside a sandbox).
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            discard_temp_attachment(state, &path);
            let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };
    let fd_list = gio::UnixFDList::new();
    let fd_index = match gio::prelude::UnixFDListExtManual::append(&fd_list, &file) {
        Ok(index) => index,
        Err(e) => {
            discard_temp_attachment(state, &path);
            let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };

    // org.freedesktop.portal.OpenURI.OpenFile(parent_window, fd, options):
    // `ask: true` makes the portal present its application chooser instead
    // of silently using the default; `handle_token` names the request (the
    // response signal is deliberately ignored - the temp file is kept until
    // Lookout exits either way). `parent_window` is left empty - a proper
    // surface handle would need XDG foreign-export plumbing per display
    // server, and portals accept an empty one.
    let options = glib::VariantDict::new(None);
    options.insert_value("handle_token", &glib::Variant::from(uuid::Uuid::new_v4().simple().to_string()));
    options.insert_value("ask", &glib::Variant::from(true));
    let options = options.end();
    let args = glib::Variant::tuple_from_iter([glib::Variant::from(""), glib::Variant::from(glib::variant::Handle(fd_index)), options]);

    let proxy = match gio::DBusProxy::for_bus_future(
        gio::BusType::Session,
        gio::DBusProxyFlags::NONE,
        None,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.OpenURI",
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(e) => {
            discard_temp_attachment(state, &path);
            let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };
    if let Err(e) = proxy
        .call_with_unix_fd_list_future("OpenFile", Some(&args), gio::DBusCallFlags::NONE, -1, Some(&fd_list))
        .await
    {
        discard_temp_attachment(state, &path);
        let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
        toast_overlay.add_toast(adw::Toast::new(&title));
    }
}
