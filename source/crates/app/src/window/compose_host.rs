//! Hosting the composer (and reopening drafts) inside the reading pane.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{AccountId, CalendarEvent, ContactsProvider, EmailAddress, EmailSummary};
use lookout_mail::session::AccountCommand;

use crate::contacts_view::{merge_contact_suggestions, SnapshotContactsProvider};
use crate::worker::Worker;

use super::*;

/// Swaps a compose widget into the reading pane's `"compose"` stack page,
/// replacing whatever was showing there (a message, or the empty
/// placeholder). Removes any leftover `"compose"` page first so repeated
/// clicks don't accumulate stale pages, and restores whatever page was
/// visible beforehand once `on_done` fires (Cancel or Send) - so Reply's
/// Discard lands back on the same message, and New mail's Discard lands
/// back on the empty placeholder. Arms the composer's header pop-out button:
/// clicking it moves the still-alive composer into its own window (see the
/// `on_pop_out` closure below), and closing that window pops the composer
/// back into this stack - the same move-in / move-out round trip as the
/// People screen's detach.
/// The action bar row below the menu bar (`view_toolbar_stack`), which hosts
/// a different command toolbar per module/ribbon tab. Its "mail-home" slot
/// normally holds the message-list command toolbar (Reply/Delete/Archive/…);
/// while a composer is embedded in the reading pane, that slot instead holds
/// the composer's editor toolbar (attach, plain/rich toggle, rich-text
/// formatting) - and the moment the composer pops out into its own window,
/// the message-list toolbar takes the slot back. This struct is the shared
/// switcher for that slot, called by `show_composer_in_reading_pane` at the
/// composer's open/close/pop-out/pop-back-in points.
pub(crate) struct ComposeActionBar {
    /// `view_toolbar_stack` - the row whose "mail-home" page swaps content.
    pub(crate) stack: gtk::Stack,
    /// The message-list command toolbar, the "mail-home" slot's normal
    /// resident; swapped back in whenever no composer is embedded.
    pub(crate) command_toolbar: gtk::Box,
}

impl ComposeActionBar {
    /// Puts `toolbar` into the stack's "mail-home" slot, evicting whatever
    /// currently occupies it (the message-list toolbar, or - after a newer
    /// composer displaced an older one - a dead composer's toolbar). The
    /// toolbar is reparented from wherever it lives (the composer's column,
    /// which GTK requires to be parent-less before `add_named`), and the
    /// slot is re-shown only if it was the visible child: if the user is on
    /// another ribbon tab or module, the swap stays silent until they
    /// return.
    pub(crate) fn install(&self, toolbar: &gtk::Box) {
        let was_visible = self.stack.visible_child_name().as_deref() == Some("mail-home");
        if let Some(current) = self.stack.child_by_name("mail-home") {
            self.stack.remove(&current);
        }
        toolbar.unparent();
        self.stack.add_named(toolbar, Some("mail-home"));
        if was_visible {
            self.stack.set_visible_child_name("mail-home");
        }
    }

    /// Puts the message-list command toolbar back into the "mail-home" slot,
    /// but only if `toolbar` (a composer's editor toolbar) currently owns it.
    /// The identity check keeps a composer that finishes while popped out
    /// (its toolbar lives in the pop-out window, not here) - or whose slot
    /// was already taken over by a newer composer - from yanking someone
    /// else's toolbar out of the bar.
    pub(crate) fn restore(&self, toolbar: &gtk::Box) {
        let owns_slot = self
            .stack
            .child_by_name("mail-home")
            .map(|current| current.downcast_ref::<gtk::Box>().is_some_and(|b| b == toolbar))
            .unwrap_or(false);
        if owns_slot {
            self.install(&self.command_toolbar);
        }
    }
}

/// Opens a saved draft in the composer (item 1.12): fetch its raw bytes
/// (`FetchRawMessage`), parse them back into a prefill, and swap the composer
/// into the reading pane. The raw round trip is what restores the draft's
/// attachments and inline images - the body cache's partial fetch never
/// downloads either - so opening always fetches (`RawMessageFetched` then
/// completes it via `open_draft_composer_with_raw`); the session's flat-file
/// raw cache makes re-opening the same draft instant. While it's in flight
/// the pane shows the loading page; a failure or timeout drops back to
/// "empty" with a toast. No-op when the message's account isn't connected.
pub(crate) fn open_draft_composer(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, summary: &EmailSummary) {
    if mailbox_account_id(&summary.mailbox).is_none() {
        return;
    }
    // A draft that is also a scheduled send (item 1.14) must not open a
    // second, independent composer - sending that would deliver it twice.
    // "Cancel send" on its banner takes it back for editing instead.
    let scheduled = {
        let st = state.borrow();
        summary
            .message_id
            .as_deref()
            .and_then(|id| scheduled_item_for_draft(&st, &summary.mailbox, crate::compose::strip_angle_brackets(id)))
            .is_some()
    };
    if scheduled {
        if let Some(overlay) = state.borrow().toast_overlay.clone() {
            overlay.add_toast(adw::Toast::new("This message is scheduled to send. Cancel send to edit it."));
        }
        return;
    }
    {
        let mut st = state.borrow_mut();
        // Refire guard: the banner and Enter/double-click can both route
        // here, and a request already in flight must not queue a second.
        if st
            .pending_draft_open
            .as_ref()
            .is_some_and(|pending| pending.mailbox == summary.mailbox && pending.uid == summary.uid)
        {
            return;
        }
        // A newer request supersedes any older one still in flight.
        st.pending_draft_open = Some(PendingDraftOpen {
            mailbox: summary.mailbox.clone(),
            uid: summary.uid,
            summary: summary.clone(),
        });
        // A body fetch the selection may have just started for this same
        // message must not render its preview over the loading page when it
        // lands - the raw fetch supersedes it (`BodyFetched` still caches
        // the body either way).
        st.pending_body_request = None;
        st.pending_html_reveal = false;
        st.reveal_generation += 1;
    }
    if !send_fetch_raw_message(state, &summary.mailbox, summary.uid) {
        state.borrow_mut().pending_draft_open = None;
        if let Some(toast_overlay) = &state.borrow().toast_overlay {
            toast_overlay.add_toast(adw::Toast::new("The draft can't be opened while the account is offline."));
        }
        return;
    }
    show_body_fetch_loading(reading_stack);
    // Backstop: a fetch lost to a reconnect must not strand the pane on its
    // spinner. Only fires while this exact request is still the outstanding
    // one.
    let state_for_timeout = state.clone();
    let reading_stack_for_timeout = reading_stack.clone();
    let mailbox_for_timeout = summary.mailbox.clone();
    let uid_for_timeout = summary.uid;
    glib::timeout_add_local_once(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let stale = {
            let mut st = state_for_timeout.borrow_mut();
            let matches = st
                .pending_draft_open
                .as_ref()
                .is_some_and(|pending| pending.mailbox == mailbox_for_timeout && pending.uid == uid_for_timeout);
            if matches {
                st.pending_draft_open = None;
            }
            matches
        };
        if stale {
            reading_stack_for_timeout.set_visible_child_name("empty");
            if let Some(toast_overlay) = &state_for_timeout.borrow().toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Opening the draft timed out"));
            }
        }
    });
}

/// Completes a draft open once its raw bytes arrive: parse them back into
/// the composer prefill and swap the composer into the reading pane. The
/// signature is deliberately skipped (`signature_kind: None`) - the saved
/// body already contains whatever signature it was written with.
pub(crate) fn open_draft_composer_with_raw(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, reading_stack: &gtk::Stack, summary: &EmailSummary, raw: &[u8]) {
    let Some(content) = lookout_mail::body::parse_draft_prefill(raw) else {
        reading_stack.set_visible_child_name("empty");
        if let Some(toast_overlay) = &state.borrow().toast_overlay {
            toast_overlay.add_toast(adw::Toast::new("This draft couldn't be displayed."));
        }
        return;
    };
    let cmd_tx = {
        let account_id = mailbox_account_id(&summary.mailbox);
        let st = state.borrow();
        account_id.and_then(|id| st.accounts.get(&id)).map(|handle| handle.cmd_tx.clone())
    };
    let Some(cmd_tx) = cmd_tx else {
        reading_stack.set_visible_child_name("empty");
        return;
    };
    let prefill = crate::compose::build_draft_prefill(summary, &content);
    // Open in rich mode exactly when the saved draft had an HTML part; a
    // plain-text draft opens plain, with its text intact.
    let rich_text_default = prefill.html_body.is_some();
    let title = summary.subject.clone().filter(|subject| !subject.trim().is_empty()).unwrap_or_else(|| "Draft".to_string());
    // The pane currently shows the fetch's loading page (swapped in over the
    // draft's preview, which the message page still holds). Point the stack
    // back at the preview before opening the composer so its Cancel/Send
    // returns there instead of a dead spinner; when nothing was rendered yet
    // (Edit via Enter on a never-previewed row), fall back to "empty".
    {
        let st = state.borrow();
        let restored = if st.rendered_message.as_ref() == Some(&(summary.mailbox.clone(), summary.uid)) {
            "message"
        } else {
            "empty"
        };
        reading_stack.set_visible_child_name(restored);
    }
    show_composer_in_reading_pane(
        state,
        worker,
        reading_stack,
        &title,
        cmd_tx,
        prefill,
        rich_text_default,
        mailbox_account_id(&summary.mailbox),
        None,
    );
}

/// The local calendar answer an "Edit response before sending" composer
/// carries (CAL-15): the invitation event with the attendee's chosen
/// `PARTSTAT`, saved through `save_replied_event` the moment Send fires -
/// the same best-effort upsert the immediate-response path runs.
pub(crate) struct ImipReplySend {
    pub(crate) calendar_state: Rc<RefCell<CalendarUiState>>,
    pub(crate) toast_overlay: adw::ToastOverlay,
    pub(crate) event: CalendarEvent,
    /// Runs after the answer is saved: the invitation surfaces for the
    /// message retire, like the immediate-response paths.
    pub(crate) on_applied: Rc<dyn Fn()>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn show_composer_in_reading_pane(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    title: &str,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill: crate::compose::ComposePrefill,
    rich_text_default: bool,
    account_id: Option<AccountId>,
    // Whether this is a new message (uses the account's "Default for new
    // messages") or a reply/forward (uses "Default for replies &
    // forwards"). The resolved signature is appended to the body on open.
    // `None` skips signature appending entirely - a reopened draft already
    // contains whatever signature it was written with.
    signature_kind: Option<crate::config_view::SignatureDefault>,
) {
    show_composer_in_reading_pane_full(
        state,
        worker,
        reading_stack,
        title,
        cmd_tx,
        prefill,
        rich_text_default,
        account_id,
        signature_kind,
        None,
        false,
    );
}

/// `show_composer_in_reading_pane` with the two settings only the invitation
/// flow needs (CAL-15): a local calendar answer applied when Send fires, and
/// opening straight into the pop-out window ("Edit response before sending").
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_composer_in_reading_pane_full(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    title: &str,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill: crate::compose::ComposePrefill,
    rich_text_default: bool,
    account_id: Option<AccountId>,
    signature_kind: Option<crate::config_view::SignatureDefault>,
    imip_reply: Option<ImipReplySend>,
    pop_out_on_open: bool,
) {
    if let Some(existing) = reading_stack.child_by_name("compose") {
        reading_stack.remove(&existing);
    }
    // Seed the delivery-receipt checkbox (C-07) from the global "request for
    // all messages I send" setting. A composer that already carries a
    // decision - a reopened draft or a withdrawn scheduled send - keeps it.
    let mut prefill = prefill;
    if prefill.request_delivery_receipt.is_none() {
        prefill.request_delivery_receipt = Some(state.borrow().settings.get_bool(crate::settings::MAIL_REQUEST_DELIVERY_RECEIPT));
    }
    let previous_page = reading_stack.visible_child_name().map(|s| s.to_string()).unwrap_or_else(|| "empty".to_string());
    let reading_stack_for_close = reading_stack.clone();
    let state_for_close = state.clone();
    // The main window's action bar, which hosts this composer's editor
    // toolbar while it's embedded. `build_window` populates it after the
    // toolbar row exists; a composer opening before then keeps its toolbar
    // inside the composer column, as before (can't happen in practice -
    // opening requires an account, which only exists once the window is
    // fully built).
    let compose_action_bar = state.borrow().compose_action_bar.clone();
    // `on_done` runs before the editor toolbar exists (it's built later,
    // alongside the composer), so it reaches the toolbar through this slot
    // instead of capturing it directly.
    let compose_toolbar: Rc<RefCell<Option<gtk::Box>>> = Rc::new(RefCell::new(None));
    let compose_action_bar_for_done = compose_action_bar.clone();
    let compose_toolbar_for_done = compose_toolbar.clone();
    // The pop-out closure below shares these with `on_done` - clone them
    // before `on_done` moves the originals in.
    let reading_stack_for_popout = reading_stack.clone();
    let state_for_popout = state.clone();
    let previous_page_for_popout = previous_page.clone();
    // Claim the draft-confirmation relay / identities-refresh slots under a
    // fresh generation. A finishing composer only clears them when its own
    // generation still owns them, so a popped-out composer that finishes
    // after a newer inline composer opened can't strip the newer one's
    // relays.
    let relay_generation = {
        let mut st = state.borrow_mut();
        st.composer_relay_generation += 1;
        st.composer_relay_generation
    };
    // True while this composer lives in its pop-out window. Set by the
    // pop-out handler, cleared by the window's close handler on pop-back-in.
    // `on_done` consults it to skip the reading-pane surgery entirely while
    // popped out: the compose page was already removed at pop-out (and any
    // page under that name now belongs to a newer composer), and there's no
    // pre-compose page to restore.
    let popped = Rc::new(Cell::new(false));
    let popped_for_popout = popped.clone();
    let on_done: Rc<dyn Fn()> = Rc::new(move || {
        // Hand the action bar back to the message-list command toolbar -
        // unless this composer's toolbar no longer owns the slot (it
        // finished while popped out, or a newer composer took the bar
        // over), which `restore` checks for.
        let toolbar = compose_toolbar_for_done.borrow().clone();
        if let (Some(bar), Some(toolbar)) = (compose_action_bar_for_done.as_ref(), toolbar) {
            bar.restore(&toolbar);
        }
        if !popped.get() {
            // Switch away from the composer's page before removing it, not
            // after: the stack's crossfade transition snapshots the
            // outgoing page to blend it out, and removing "compose" first
            // (only then switching) tore the widget out from under that
            // snapshot, leaving a stale ghost of its card-styled fields
            // group painted over the background once the transition
            // finished. Restore the pre-compose page only if the pane is
            // still on the composer.
            if reading_stack_for_close.visible_child_name().as_deref() == Some("compose") {
                reading_stack_for_close.set_visible_child_name(&previous_page);
            }
            if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
                reading_stack_for_close.remove(&existing);
            }
        }
        let mut st = state_for_close.borrow_mut();
        if st.composer_relay_generation == relay_generation {
            // The composer is gone; drop its draft-confirmation relay so its
            // consumer future exits and late events go nowhere, and its
            // identities-refresh hook so a later Config manage-identities
            // edit doesn't poke a dead dropdown.
            st.draft_saved_tx = None;
            st.composer_identities_refresh = None;
        }
        // Close this composer's own pop-out window when it finishes -
        // Cancel/Send in the window must take it down, not leave a dead
        // composer on screen. Guarded by the window's own generation, which
        // is independent of the relay ownership above: a popped-out composer
        // finishing after a newer inline composer opened still closes its
        // own window.
        let owns_window = st.compose_popout_window.as_ref().map(|(gen, _)| *gen == relay_generation).unwrap_or(false);
        if owns_window {
            if let Some((_, win)) = st.compose_popout_window.take() {
                win.destroy();
            }
        }
    });
    // The header's pop-out button hands the composer to this closure, which
    // moves it into its own window - the composer itself stays alive (draft
    // autosave included), so nothing needs rebuilding or re-seeding. The
    // composer set `moving`/`popped_out` around the removal, which the
    // autosave's `root-notify` guard consults so the move isn't mistaken for
    // a displacement. Closing the window pops the composer back into the
    // reading pane (same `"compose"` page, displacing any newer composer
    // that opened meanwhile), unless Send/Cancel already finished the
    // session - then the window just closes.
    let on_pop_out: Option<Rc<dyn Fn(crate::compose::PopOutHandle)>> = Some(Rc::new({
        let state_for_close = state_for_popout;
        let reading_stack_for_close = reading_stack_for_popout;
        let previous_page = previous_page_for_popout;
        let title = title.to_string();
        let popped = popped_for_popout;
        let compose_action_bar = compose_action_bar.clone();
        move |handle| {
            popped.set(true);
            // Switch away from "compose" before removing it - see the
            // matching note in `on_done` above for why the order matters
            // (a stale crossfade snapshot ghosting over the background).
            if reading_stack_for_close.visible_child_name().as_deref() == Some("compose") {
                reading_stack_for_close.set_visible_child_name(&previous_page);
            }
            if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
                reading_stack_for_close.remove(&existing);
            }
            // The composer's editor toolbar lives in the main window's
            // action bar while it's embedded (see `ComposeActionBar`);
            // return it to the composer column - where it rides into the
            // pop-out window below - and restore the message-list command
            // toolbar to the action bar. Once out of the ribbon strip it
            // loses that background, so the toolbar gets the ribbon's own
            // strip treatment (the same `window-icon-toolbar-background`
            // class as the action bar row) - its icons keep the same
            // background and dimensions as the ribbon's in the editor
            // window too.
            if let Some(bar) = &compose_action_bar {
                bar.restore(&handle.editor_toolbar);
                handle.editor_toolbar.add_css_class("window-icon-toolbar-background");
                handle.widget.insert_child_after(&handle.editor_toolbar, Some(&handle.toolbar_anchor));
            }
            // A header bar of its own is what gives the window a drag
            // region - a bare `adw::Window` has none (the People pop-out
            // needed the same, see 0.8.0).
            let win = adw::Window::builder().default_width(860).default_height(640).build();
            let header = adw::HeaderBar::new();
            header.set_title_widget(Some(&adw::WindowTitle::new(&title, "")));
            // Cancel, Send, From and the read-receipt toggle live in the
            // title bar while popped out - the composer's own header row is
            // redundant once the window has a title bar (and is hidden
            // wholesale, see compose.rs's `root-notify` handler) - and are
            // moved back on pop-back-in (see the close handler below).
            // They're the same widgets, so their handlers keep working
            // across the move. The draft status label moves into the title
            // bar with them; the title label stays hidden with its row.
            handle.top_row.remove(&handle.cancel_button);
            handle.top_row.remove(&handle.send_button);
            handle.top_row.remove(&handle.status_label);
            handle.top_row.remove(&handle.from_dropdown);
            handle.top_row.remove(&handle.read_receipt_toggle);
            handle.top_row.remove(&handle.delivery_receipt_toggle);
            handle.top_row.remove(&handle.light_view_button);
            // A button to pop the composer back into the reading pane
            // without closing the session: it closes the window, and the
            // close handler below performs the pop-back-in (the same path
            // as the window's own close button, minus the "already
            // finished" branch).
            let back_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popin1", "go-previous-symbolic", "go-back-symbolic"]));
            back_button.set_tooltip_text(Some("Back to main window"));
            {
                let win = win.clone();
                back_button.connect_clicked(move |_| win.close());
            }
            // Header bar packing order: start-side children are prepended
            // (each new call lands leftmost), end-side children are appended
            // outward (each new call lands further right), so this yields,
            // left to right: [from, send | title | status, cancel, back,
            // read receipt, light view] - "View in Light Mode" is now the
            // outermost widget on the right.
            header.pack_start(&handle.send_button);
            header.pack_start(&handle.from_dropdown);
            header.pack_end(&handle.status_label);
            header.pack_end(&handle.cancel_button);
            header.pack_end(&back_button);
            header.pack_end(&handle.read_receipt_toggle);
            header.pack_end(&handle.delivery_receipt_toggle);
            header.pack_end(&handle.light_view_button);
            let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
            content_box.append(&header);
            content_box.append(&handle.widget);
            win.set_content(Some(&content_box));
            // Held here (not just by the closure below) so the window
            // survives after this handler returns - GTK only keeps a
            // presented window alive while it's on screen.
            state_for_close.borrow_mut().compose_popout_window = Some((relay_generation, win.clone()));
            {
                let state_for_close = state_for_close.clone();
                let reading_stack_for_close = reading_stack_for_close.clone();
                let popped = popped.clone();
                let compose_action_bar = compose_action_bar.clone();
                let handle = handle;
                win.connect_close_request(move |_| {
                    if !handle.done.get() {
                        handle.moving.set(true);
                        handle.popped_out.set(false);
                        if let Some(parent) = handle.widget.parent().and_then(|parent| parent.downcast::<gtk::Box>().ok()) {
                            parent.remove(&handle.widget);
                        }
                        // Send Cancel, Send, From and the read-receipt
                        // toggle home to the composer's header row, out of
                        // the title bar they're about to leave. `pack_start`/
                        // `pack_end` parent a widget to an internal box
                        // inside `AdwHeaderBar`, not the header bar itself,
                        // so a type-checked removal off the header bar never
                        // matches - `unparent()` detaches a widget from
                        // whatever its actual current parent is.
                        handle.cancel_button.unparent();
                        handle.send_button.unparent();
                        handle.status_label.unparent();
                        handle.from_dropdown.unparent();
                        handle.read_receipt_toggle.unparent();
                        handle.delivery_receipt_toggle.unparent();
                        handle.light_view_button.unparent();
                        handle.top_row.prepend(&handle.from_dropdown);
                        handle.top_row.prepend(&handle.send_button);
                        handle.top_row.append(&handle.cancel_button);
                        handle.top_row.append(&handle.status_label);
                        handle.top_row.reorder_child_after(&handle.status_label, Some(&handle.title_label));
                        handle.top_row.append(&handle.read_receipt_toggle);
                        handle.top_row.append(&handle.delivery_receipt_toggle);
                        handle.top_row.append(&handle.light_view_button);
                        handle.top_row.reorder_child_after(&handle.light_view_button, Some(&handle.status_label));
                        handle.top_row.reorder_child_after(&handle.read_receipt_toggle, Some(&handle.light_view_button));
                        handle.top_row.reorder_child_after(&handle.delivery_receipt_toggle, Some(&handle.read_receipt_toggle));
                        if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
                            reading_stack_for_close.remove(&existing);
                        }
                        reading_stack_for_close.add_named(&handle.widget, Some("compose"));
                        // The editor toolbar rode into the pop-out window
                        // inside the composer; lift it back into the main
                        // window's action bar (see `ComposeActionBar`) now
                        // that the composer is embedded again. `install`
                        // evicts whatever currently owns the slot - a newer
                        // composer's toolbar, if one opened meanwhile. The
                        // pop-out's own strip styling comes off with it: the
                        // action bar's strip provides that background again.
                        if let Some(bar) = &compose_action_bar {
                            handle.editor_toolbar.remove_css_class("window-icon-toolbar-background");
                            bar.install(&handle.editor_toolbar);
                        }
                        handle.moving.set(false);
                        popped.set(false);
                        reading_stack_for_close.set_visible_child_name("compose");
                    }
                    state_for_close.borrow_mut().compose_popout_window = None;
                    glib::Propagation::Proceed
                });
            }
            win.present();
        }
    }));
    // Recipient autocomplete combines local mail-history addresses with
    // CardDAV contacts discovered for this account. CardDAV is queried from
    // UI memory synchronously (cheap, no SQLite); the mail-history half is
    // an off-thread cache read (`spawn_cache_read`) - the completion callback
    // fires once it answers, merged with the CardDAV half computed up front.
    let address_cache = account_id.clone().and_then(|id| state.borrow().accounts.get(&id).and_then(|h| h.address_cache.clone()));
    let carddav_provider = SnapshotContactsProvider {
        state: state.clone(),
        account: account_id.clone(),
    };
    let worker_for_suggestions = worker.clone();
    let state_for_suggestions = state.clone();
    let account_for_suggestions = account_id.clone();
    // The directory source this composer's To/Cc/Bcc fields (and its recipient
    // picker, via `directory_lookup` below) search (item 4.7.6): scoped to the
    // account being sent from, so only that account's directories answer.
    let directory = crate::directory::directory_source(state, account_id.clone(), worker.clone());
    let directory_lookup = directory.suggestion_lookup();
    let suggestions: crate::recipient_entry::SuggestionSource = Rc::new(move |prefix: &str, complete: Rc<dyn Fn(Vec<crate::recipient_entry::Suggestion>)>| {
        let carddav = carddav_provider.search_contacts(prefix, 8);
        // Contact lists (P-05) live on the same account snapshots the picker
        // reads; collect the ones whose name matches the typed prefix.
        let carddav_lists: Vec<lookout_core::ContactList> = {
            let st = state_for_suggestions.borrow();
            match &account_for_suggestions {
                Some(id) => st.contacts_by_account.get(id).map(|snapshot| snapshot.contact_lists.clone()).unwrap_or_default(),
                None => st.contacts_by_account.values().flat_map(|snapshot| snapshot.contact_lists.clone()).collect(),
            }
        };
        // The directory half is a network round trip. Kick it off beside the
        // (fast) mail-history read and deliver the merged result when it lands,
        // replacing the stored-contact-only delivery that went out first. A
        // cached prefix answers before this returns, so the two deliveries can
        // land in either order - the source's own keystroke generation guard
        // plus `RecipientEntry`'s ensure a later prefix always wins.
        let prefix = prefix.trim().to_string();
        let deliver_directory = {
            let directory = directory.clone();
            let complete = complete.clone();
            let carddav = carddav.clone();
            let carddav_lists = carddav_lists.clone();
            let prefix = prefix.clone();
            move |mail_history: Vec<EmailAddress>| {
                directory.search(
                    &prefix,
                    Rc::new({
                        let complete = complete.clone();
                        let carddav = carddav.clone();
                        let carddav_lists = carddav_lists.clone();
                        let prefix = prefix.clone();
                        move |hits| complete(merge_contact_suggestions(mail_history.clone(), &carddav, &hits, &carddav_lists, &prefix, 8))
                    }),
                );
            }
        };
        let Some(cache) = address_cache.clone() else {
            complete(merge_contact_suggestions(Vec::new(), &carddav, &[], &carddav_lists, &prefix, 8));
            deliver_directory(Vec::new());
            return;
        };
        let reply_rx = spawn_cache_read(&worker_for_suggestions, cache, {
            let prefix = prefix.clone();
            move |cache| cache.search_contacts(&prefix, 8)
        });
        glib::spawn_future_local(async move {
            let mail_history = reply_rx.recv().await.unwrap_or_default();
            complete(merge_contact_suggestions(mail_history.clone(), &carddav, &[], &carddav_lists, &prefix, 8));
            deliver_directory(mail_history);
        });
    });
    // Shows the "Sending: <subject>" toast the instant Send is clicked - with
    // Undo for the undo-send delay (item 1.13) - and registers the send in
    // `pending_sends` so the account event loop can retract the toast once it
    // completes, fails or is cancelled (see `AccountEvent::OutboxQueued` and
    // friends in `connect_account`).
    let on_send_started: crate::compose::SendStarted = {
        let state = state.clone();
        let account_id = account_id.clone();
        // The answer is applied once, on the first Send - a second Send
        // click would be a no-op anyway, but the flag keeps it from writing
        // the same PARTSTAT twice.
        let reply_applied = Rc::new(Cell::new(false));
        Rc::new(move |message: &lookout_mail::ComposedMessage, schedule| {
            if let Some(reply) = &imip_reply {
                if !reply_applied.replace(true) {
                    save_replied_event(&reply.calendar_state, &reply.toast_overlay, reply.event.clone());
                    (reply.on_applied)();
                }
            }
            start_send_feedback(&state, account_id.clone(), message, schedule)
        })
    };
    // Backs the To row's address-book button: opens `contact_picker` over
    // whichever window currently hosts the composer (main window or a
    // popped-out compose window, resolved from `anchor` at click time), and
    // hands back whatever the user checked.
    let open_contact_picker: crate::contact_picker::ContactPickerOpener = {
        let state = state.clone();
        // Hand the picker the same account-scoped directory source the To/Cc/
        // Bcc autocomplete uses (item 4.7.7), so its Directory section shares
        // this composer's prefix cache and scope.
        let directory_lookup = directory_lookup.clone();
        Rc::new(move |anchor, on_add| crate::contact_picker::show_contact_picker(&anchor, state.clone(), Some(directory_lookup.clone()), on_add))
    };
    // The composer's ribbon shares the window's layout state, so a layout
    // switch reaches it too. `unwrap_or_else` only guards a build order where
    // `build_window` hasn't stored it yet.
    let ribbon_style = state
        .borrow()
        .ribbon_style
        .clone()
        .unwrap_or_else(|| crate::ribbon::RibbonStyle::new(crate::ribbon::RibbonMode::default()));
    let (composer, editor_toolbar, draft_tx, identities_refresh) = crate::compose::build_compose_view(
        title,
        // The composer's From dropdown re-reads from here whenever the
        // Config → Mail accounts manage-identities dialog fires `on_changed`
        // (see `composer_identities_refresh`), so edits made while a
        // composer is open are live. Falls back to the first connected
        // account when no explicit account was resolved.
        {
            let state = state.clone();
            let account_id = account_id.clone();
            Rc::new(move || {
                let st = state.borrow();
                let resolved = account_id.clone().or_else(|| st.accounts.keys().next().cloned());
                match resolved {
                    Some(id) => match st.accounts.get(&id) {
                        Some(handle) => st.app_config.borrow().identities_for_account(&id, &handle.display_name, &handle.email),
                        None => Vec::new(),
                    },
                    None => Vec::new(),
                }
            })
        },
        // The composer's signature menu re-reads the global signature list
        // each time it opens, so a signature added via Settings while a
        // composer is open shows up in its list immediately.
        {
            let state = state.clone();
            Rc::new(move || state.borrow().app_config.borrow().signatures.clone())
        },
        // "Manage Signatures…" opens File ▸ Options on the Accounts section;
        // set by `build_window` once the File page exists (see
        // `UiState::manage_signatures`).
        state.borrow().manage_signatures.clone(),
        // The composer's My Templates menu re-reads the global template list
        // each time it opens (item 2.10 / C-17), like the signature menu.
        {
            let state = state.clone();
            Rc::new(move || state.borrow().app_config.borrow().templates.clone())
        },
        // "Manage Templates…" opens File ▸ Options on the Mail section; set
        // by `build_window` once the File page exists (see
        // `UiState::manage_templates`).
        state.borrow().manage_templates.clone(),
        // The account's default signature for this compose kind, resolved
        // from the per-account defaults (falling back to the first
        // connected account when no explicit account was resolved, exactly
        // like the From dropdown above). Appended to the body on open.
        {
            let st = state.borrow();
            let resolved = account_id.clone().or_else(|| st.accounts.keys().next().cloned());
            let config = st.app_config.borrow();
            let id = resolved.and_then(|id| config.signature_defaults.get(&id)).and_then(|defaults| match signature_kind {
                Some(crate::config_view::SignatureDefault::New) => defaults.new,
                Some(crate::config_view::SignatureDefault::Reply) => defaults.reply,
                None => None,
            });
            id.and_then(|id| config.signatures.iter().find(|s| s.id == id).cloned())
        },
        cmd_tx,
        prefill,
        on_done,
        rich_text_default,
        state.borrow().settings.get_bool(crate::settings::MAIL_SPELL_CHECK),
        state.borrow().settings.get_strv(crate::settings::MAIL_SPELL_CHECK_LANGUAGES),
        state.borrow().settings.get_bool(crate::settings::MAIL_WARN_NO_ATTACHMENT),
        suggestions,
        open_contact_picker,
        on_pop_out,
        on_send_started,
        // Scheduling on a Microsoft 365 account with Graph access goes
        // server-side through Exchange (item 5.5 / C-08) so it goes out even
        // with Lookout closed; otherwise this returns false and the local
        // outbox handles it.
        {
            let state = state.clone();
            let account_id = account_id.clone();
            Some(Rc::new(move |message: &lookout_mail::ComposedMessage, at: chrono::DateTime<chrono::Utc>| {
                let resolved = {
                    let st = state.borrow();
                    account_id.clone().or_else(|| st.accounts.keys().next().cloned())
                };
                match resolved {
                    Some(id) => schedule_send_via_graph(&state, &id, message, at),
                    None => false,
                }
            }))
        },
        pop_out_on_open,
        ribbon_style,
    );
    // Replacing any previous composer's relay (dropped sender = its consumer
    // exits), and hooking the Config manage-identities dialog into the new
    // composer's From dropdown.
    state.borrow_mut().draft_saved_tx = Some(draft_tx);
    state.borrow_mut().composer_identities_refresh = Some(identities_refresh);
    // The composer is embedded, so its editor toolbar moves out of the
    // composer column and into the main window's action bar, replacing the
    // message-list command toolbar (see `ComposeActionBar`). Filled into the
    // shared slot afterwards so `on_done` can hand the bar back on
    // Cancel/Send.
    if let Some(bar) = &compose_action_bar {
        bar.install(&editor_toolbar);
    }
    *compose_toolbar.borrow_mut() = Some(editor_toolbar);
    reading_stack.add_named(&composer, Some("compose"));
    reading_stack.set_visible_child_name("compose");
}
