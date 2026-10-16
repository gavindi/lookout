//! "Message in new window" (item 1.9 / M-31): a standalone, read-only
//! message window, opened by double-click / Enter on a message-list row or
//! the reading pane's "More" menu ("Open in new window"), and closed with
//! Esc or the window's close button.
//!
//! The window deliberately does *not* share the reading pane's rendering
//! path (`render_body` in `window.rs`): that path is built around
//! single-slot `UiState` state and a `cid:` URI scheme registered once per
//! WebKit context against `rendered_message`, so a second concurrently
//! visible pane cannot safely reuse it. Instead the window renders
//! self-contained from the message's raw bytes - `FetchRawMessage` (already
//! disk-cached in the session), `lookout_mail::body::parse_body`, and
//! `rewrite_cid_refs_to_data_uris` for inline images - applying the same
//! external-content policy (`window::remote_content_policy` +
//! `window::wrap_message_with_csp`) and theme override as the reading pane.
//! When the reading pane has already fetched the body and it carries no
//! `cid:` references, the body cache is used directly and no fetch is
//! needed; the raw bytes are only essential for inline images.
//!
//! One window per `(mailbox, uid)` - re-opening an already-open message
//! re-presents its window. The registry (`UiState::message_windows`) keeps
//! the window alive and lets `AccountEvent::RawMessageFetched` route bytes
//! to a window whose fetch is still outstanding (`deliver_raw`), and lets
//! the attachment Open/Save flows treat a pop-out message as "on screen"
//! (`window::message_is_visible`).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{EmailBody, EmailSummary, MailboxId, Uid};
use webkit::prelude::*;

use crate::window::UiState;
use crate::worker::Worker;

/// One open "message in new window" surface. Lives in
/// `UiState::message_windows` under its `(mailbox, uid)` key - the strong
/// `Rc` is what keeps the GTK window alive while it's presented.
pub(crate) struct MessageWindowState {
    /// The window itself. Held so `open_message_window` can re-present it
    /// and the close handler can compare identity.
    window: adw::Window,
    mailbox: MailboxId,
    uid: Uid,
    summary: EmailSummary,
    /// Whether a body has been rendered into the window yet. `deliver_raw`
    /// only serves windows still waiting on their first render.
    rendered: bool,
    /// This window's own "load remote images once" override - the
    /// equivalent of the reading pane's `load_once_images`, kept per window
    /// so the remote-content banner's "Load images" action doesn't affect
    /// the main pane's message.
    load_images_once: bool,
    /// The external-content policy in force for the current render, kept as
    /// shared cells so the WebView's decide-policy handler reads the same
    /// values the CSP was built from (the cells are cheap to update on every
    /// render; the handler is connected once at build time).
    allow_images: Rc<Cell<bool>>,
    allow_all: Rc<Cell<bool>>,
    /// The last raw bytes rendered, kept so the theme toggle and the
    /// remote-content banner can re-render without another fetch. `None`
    /// when the window rendered from the body cache (no `cid:` references,
    /// so no raw was needed); a re-render then works from `last_body`.
    last_raw: Option<Vec<u8>>,
    /// The last parsed body, for re-renders and for the reply/forward
    /// prefill when the body cache has already evicted it.
    last_body: Option<EmailBody>,
    header: crate::message_header::MessageHeader,
    web_view: webkit::WebView,
    content_stack: gtk::Stack,
    text_view: gtk::TextView,
    remote_banner: adw::Banner,
    auth_banner: adw::Banner,
    external_banner: adw::Banner,
    sensitivity_banner: adw::Banner,
    page_stack: gtk::Stack,
    error_label: gtk::Label,
}

/// Opens (or re-presents) the message window for `summary`. A no-op when the
/// message's account isn't connected - same convention as the reading pane's
/// reply handlers.
pub(crate) fn open_message_window(parent: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, main_reading_stack: &gtk::Stack, summary: &EmailSummary) {
    let key = (summary.mailbox.clone(), summary.uid);
    if let Some(existing) = state.borrow().message_windows.get(&key).cloned() {
        existing.borrow().window.present();
        return;
    }
    if crate::window::mailbox_account_id(&summary.mailbox).is_none() {
        return;
    }

    let subject = summary.subject.clone().unwrap_or_else(|| "(no subject)".to_string());
    let sender = summary.from.first().map(|a| a.display_label().to_string()).unwrap_or_default();

    let window = adw::Window::builder().default_width(900).default_height(700).title(&subject).build();
    let header_bar = adw::HeaderBar::new();
    header_bar.set_title_widget(Some(&adw::WindowTitle::new(&subject, &sender)));
    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.set_css_classes(&["flat"]);
    close_button.set_tooltip_text(Some("Close"));
    header_bar.pack_end(&close_button);

    let header = crate::message_header::build();

    // --- Body: WebKit for HTML, GtkTextView for plain text - the reading
    // pane's widget stack, minus the banners the window doesn't offer
    // (unsubscribe / iMIP / read receipt). ---
    let webkit_settings = webkit::Settings::new();
    webkit_settings.set_enable_javascript(false);
    webkit_settings.set_enable_developer_extras(false);
    let user_content_manager = webkit::UserContentManager::new();
    let theme_override_sheet = webkit::UserStyleSheet::new(
        crate::window::MESSAGE_THEME_OVERRIDE_CSS,
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
    // The popped-out message body: blurred by "All text" mode like the
    // reading pane's.
    crate::privacy::mark_blur_all(&web_view);
    crate::window::install_web_view_zoom(&web_view);
    let allow_images: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let allow_all: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    {
        // Mirror of the reading pane's policy handler, reading this window's
        // own policy cells. The CSP injected at render is authoritative for
        // remote subresources (WebKitGTK only delivers response decisions
        // for the main frame); this branch is defence-in-depth plus the
        // navigation and new-window handling.
        let allow_images = allow_images.clone();
        let allow_all = allow_all.clone();
        web_view.connect_decide_policy(move |_view, decision, decision_type| {
            let uri_is_local = |uri: &str| -> bool { matches!(uri.split(':').next().unwrap_or(""), "data" | "cid" | "about" | "file") };
            match decision_type {
                webkit::PolicyDecisionType::NavigationAction => {
                    let navigation = decision.downcast_ref::<webkit::NavigationPolicyDecision>().and_then(|d| d.navigation_action());
                    let is_link_click = navigation.as_ref().map(|a| a.navigation_type() == webkit::NavigationType::LinkClicked).unwrap_or(false);
                    let is_user_gesture = navigation.as_ref().map(|a| a.is_user_gesture()).unwrap_or(false);
                    if is_link_click || is_user_gesture {
                        if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                            open_uri_in_default_browser(&uri);
                        }
                        decision.ignore();
                        return true;
                    }
                    if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                        if !uri_is_local(&uri) {
                            decision.ignore();
                            return true;
                        }
                    }
                }
                webkit::PolicyDecisionType::NewWindowAction => return false,
                webkit::PolicyDecisionType::Response => {
                    if let Some(response) = decision.downcast_ref::<webkit::ResponsePolicyDecision>() {
                        if !response.is_main_frame_main_resource() {
                            if let Some(uri) = response.request().and_then(|r| r.uri()) {
                                if !uri_is_local(&uri) {
                                    let is_image = response.response().and_then(|r| r.mime_type()).is_some_and(|m| m.starts_with("image/"));
                                    if !((allow_images.get() && is_image) || allow_all.get()) {
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
    }
    let web_view_for_create = web_view.clone();
    web_view_for_create.connect_create(move |_view, navigation_action| {
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
    let content_stack = gtk::Stack::new();
    content_stack.set_widget_name("body");
    content_stack.set_vexpand(true);
    content_stack.add_named(&web_view, Some("html"));
    content_stack.add_named(&text_scroller, Some("text"));

    let remote_banner = adw::Banner::new("Remote content is blocked");
    remote_banner.set_button_label(Some("Load images"));
    remote_banner.set_revealed(false);

    // Sender-authentication (6.1), external-sender (6.2) and sensitivity
    // (6.5) info bars, mirroring the main reading pane's. No buttons.
    let auth_banner = adw::Banner::new("");
    auth_banner.set_revealed(false);
    let external_banner = adw::Banner::new("This sender is external");
    external_banner.set_revealed(false);
    let sensitivity_banner = adw::Banner::new("");
    sensitivity_banner.set_revealed(false);

    let attachments = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    attachments.set_widget_name("attachments");
    attachments.set_visible(false);

    // The "message" page has the same shape `rebuild_attachment_strip` and
    // `find_named_child` expect from the reading pane: a `gtk::Box` named
    // "message" whose direct children include the "attachments" strip and
    // the "body" content stack.
    let message_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_page.append(&remote_banner);
    message_page.append(&auth_banner);
    message_page.append(&external_banner);
    message_page.append(&sensitivity_banner);
    message_page.append(&attachments);
    message_page.append(&content_stack);

    let loading_page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    let spinner = gtk::Spinner::new();
    spinner.start();
    loading_page.append(&spinner);
    loading_page.append(&gtk::Label::builder().label("Loading message…").css_classes(["dim-label"]).build());

    let error_label = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["dim-label"]).build();
    let retry_button = gtk::Button::with_label("Retry");
    retry_button.add_css_class("suggested-action");
    let error_page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    error_page.append(&error_label);
    error_page.append(&retry_button);

    let page_stack = gtk::Stack::new();
    page_stack.set_transition_type(gtk::StackTransitionType::None);
    page_stack.set_vexpand(true);
    page_stack.add_named(&message_page, Some("message"));
    page_stack.add_named(&loading_page, Some("loading"));
    page_stack.add_named(&error_page, Some("error"));
    page_stack.set_visible_child_name("loading");

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&header_bar);
    content.append(&header.subject_bar);
    content.append(&header.widget);
    content.append(&page_stack);
    content.append(&header.action_bar);
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&content));
    window.set_content(Some(&toast_overlay));

    // Theme: start in the configured default and arm the physical override
    // before the toggle handler exists, so the handler's first `toggled`
    // can't fire for the initial state.
    let initial_dark = state.borrow().message_theme_dark_default();
    header.theme_button.set_active(initial_dark);
    crate::window::set_message_theme_armed(initial_dark, &web_view, &user_content_manager, &theme_override_sheet);

    // The initial body: render straight from the cache when it's already
    // known and carries no `cid:` references (nothing needs the raw bytes);
    // otherwise fetch the raw message. `parse_body` and the raw fetch both
    // live behind `deliver_raw` for the asynchronous case.
    let cached = state
        .borrow_mut()
        .cached_body(&summary.mailbox, summary.uid)
        .filter(|body| body.html_body.as_deref().is_none_or(|html| !html.contains("cid:")));

    let entry = Rc::new(RefCell::new(MessageWindowState {
        window: window.clone(),
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
        summary: summary.clone(),
        rendered: false,
        load_images_once: false,
        allow_images: allow_images.clone(),
        allow_all: allow_all.clone(),
        last_raw: None,
        last_body: None,
        header: header.clone(),
        web_view: web_view.clone(),
        content_stack: content_stack.clone(),
        text_view: text_view.clone(),
        remote_banner: remote_banner.clone(),
        auth_banner: auth_banner.clone(),
        external_banner: external_banner.clone(),
        sensitivity_banner: sensitivity_banner.clone(),
        page_stack: page_stack.clone(),
        error_label: error_label.clone(),
    }));
    state.borrow_mut().message_windows.insert(key.clone(), entry.clone());

    // Reveal the message page only once WebKit has finished painting the
    // body, so the window never flashes a half-loaded document (text bodies
    // set the page directly in `render_data`).
    {
        let entry = entry.clone();
        web_view.connect_load_changed(move |_view, event| {
            if event == webkit::LoadEvent::Finished {
                entry.borrow().page_stack.set_visible_child_name("message");
            }
        });
    }

    // Close bookkeeping: drop the registry entry (and with it the window's
    // strong reference) and stop the entry from being re-presented.
    {
        let state = state.clone();
        let key_for_close = key.clone();
        window.connect_close_request(move |_| {
            state.borrow_mut().message_windows.remove(&key_for_close);
            glib::Propagation::Proceed
        });
    }
    {
        let window = window.clone();
        close_button.connect_clicked(move |_| window.close());
    }
    {
        // Esc closes the message window (parity for Outlook's popped-out
        // item); the reading pane's own Escape behaviour is unchanged.
        let keys = gtk::EventControllerKey::new();
        {
            let window = window.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    window.close();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(keys);
    }

    // --- Header actions, resolved against this window's own message (not
    // the list selection, which may have moved on). ---
    for (button, mode, title) in [
        (&header.reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&header.reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
        (&header.bottom_reply_button, crate::compose::ReplyMode::Reply, "Reply"),
    ] {
        wire_reply_button(button, mode, title, &entry, state, worker, main_reading_stack);
    }
    for button in [&header.forward_button, &header.bottom_forward_button] {
        wire_forward_button(button, &entry, state, worker, main_reading_stack);
    }
    {
        let entry = entry.clone();
        let state = state.clone();
        let parent = parent.clone();
        let toast_overlay = toast_overlay.clone();
        header.contact_button.clone().connect_clicked(move |_| {
            let summary = entry.borrow().summary.clone();
            let Some(sender) = summary.from.first() else { return };
            let account_id = crate::window::mailbox_account_id(&summary.mailbox);
            match crate::contacts_view::find_contact_by_address(&state, &sender.address, account_id.as_ref()) {
                Some((_account_id, contact)) => crate::contacts_view::show_contact_editor_for(&parent, &state, &toast_overlay, &contact),
                None => crate::contacts_view::show_create_contact_for(&parent, &state, &toast_overlay, sender, account_id),
            }
        });
    }
    {
        let entry = entry.clone();
        let state = state.clone();
        let web_view = web_view.clone();
        let user_content_manager = user_content_manager.clone();
        let theme_override_sheet = theme_override_sheet.clone();
        header.theme_button.clone().connect_toggled(move |button| {
            button.set_icon_name(if button.is_active() {
                crate::window::themed_icon_name(&["weather-clear-symbolic", "display-brightness-symbolic"])
            } else {
                crate::window::themed_icon_name(&["weather-clear-night-symbolic", "night-light-symbolic", "weather-clear-symbolic"])
            });
            button.set_tooltip_text(Some(if button.is_active() { "View in Light Mode" } else { "View in Dark Mode" }));
            crate::window::set_message_theme_armed(button.is_active(), &web_view, &user_content_manager, &theme_override_sheet);
            rerender(&state, &entry);
        });
    }
    {
        // The remote-content banner is this window's "load once" affordance;
        // the trust settings it consults are the same the reading pane uses.
        let entry = entry.clone();
        let state = state.clone();
        remote_banner.connect_button_clicked(move |banner| {
            banner.set_revealed(false);
            entry.borrow_mut().load_images_once = true;
            rerender(&state, &entry);
        });
    }
    {
        let entry = entry.clone();
        let state = state.clone();
        let page_stack = page_stack.clone();
        let error_label = error_label.clone();
        retry_button.connect_clicked(move |_| {
            let (mailbox, uid) = {
                let e = entry.borrow();
                (e.mailbox.clone(), e.uid)
            };
            if crate::window::send_fetch_raw_message(&state, &mailbox, uid) {
                page_stack.set_visible_child_name("loading");
            } else {
                error_label.set_text("The message can't be loaded while the account is offline.");
                page_stack.set_visible_child_name("error");
            }
        });
    }

    if let Some(body) = cached {
        render_data(state, &entry, &body, None);
    } else if crate::window::send_fetch_raw_message(state, &summary.mailbox, summary.uid) {
        page_stack.set_visible_child_name("loading");
        // Backstop for a fetch lost to a reconnect: never leave the window
        // on its spinner. Only fires while this window still owns an
        // unrendered request for this message.
        let state_for_timeout = state.clone();
        let key_for_timeout = key.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(crate::window::ATTACHMENT_FETCH_TIMEOUT_MS), move || {
            let still_waiting = {
                let st = state_for_timeout.borrow();
                st.message_windows.get(&key_for_timeout).is_some_and(|entry| !entry.borrow().rendered)
            };
            if still_waiting {
                let entry = state_for_timeout.borrow().message_windows.get(&key_for_timeout).cloned();
                if let Some(entry) = entry {
                    show_error(&entry, "Loading this message timed out.");
                }
            }
        });
    } else {
        error_label.set_text("The message can't be loaded while the account is offline.");
        page_stack.set_visible_child_name("error");
    }

    window.present();
}

/// Opens the default browser for an `http(s)` URI, ignoring other schemes -
/// same behaviour as the reading pane's link handling.
fn open_uri_in_default_browser(uri: &str) {
    let scheme = uri.split(':').next().unwrap_or("");
    if matches!(scheme, "http" | "https") {
        if let Err(e) = gtk::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk::gio::AppLaunchContext>) {
            tracing::warn!("failed to open link {uri} in the default browser: {e}");
        }
    }
}

/// Routes a fetched raw message to the window for `(mailbox, uid)` when one
/// is open and still waiting on its first render. Returns whether the bytes
/// were consumed. Called from the account event loop, where the window's own
/// `FetchRawMessage` response arrives.
pub(crate) fn deliver_raw(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, bytes: &[u8]) -> bool {
    let entry = state.borrow().message_windows.get(&(mailbox.clone(), uid)).cloned();
    let Some(entry) = entry else { return false };
    if entry.borrow().rendered {
        return false;
    }
    render_raw(state, &entry, bytes);
    true
}

/// Shows the error page of the window for `(mailbox, uid)` when one is open
/// and still waiting on its first render. Returns whether the error was
/// consumed.
pub(crate) fn deliver_error(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, message: &str) -> bool {
    let entry = state.borrow().message_windows.get(&(mailbox.clone(), uid)).cloned();
    let Some(entry) = entry else { return false };
    if entry.borrow().rendered {
        return false;
    }
    show_error(&entry, &format!("This message couldn't be loaded: {message}"));
    true
}

fn show_error(entry: &Rc<RefCell<MessageWindowState>>, message: &str) {
    let (error_label, page_stack) = {
        let e = entry.borrow();
        (e.error_label.clone(), e.page_stack.clone())
    };
    error_label.set_text(message);
    page_stack.set_visible_child_name("error");
}

/// Re-renders the current message from the bytes/body this window already
/// holds - the theme toggle and the remote-content banner both change the
/// render inputs and must apply to the message on screen.
fn rerender(state: &Rc<RefCell<UiState>>, entry: &Rc<RefCell<MessageWindowState>>) {
    let (raw, body) = {
        let e = entry.borrow();
        (e.last_raw.clone(), e.last_body.clone())
    };
    if let Some(raw) = raw {
        render_raw(state, entry, &raw);
    } else if let Some(body) = body {
        render_data(state, entry, &body, None);
    }
}

/// Parses and renders raw RFC 5322 bytes into the window.
fn render_raw(state: &Rc<RefCell<UiState>>, entry: &Rc<RefCell<MessageWindowState>>, raw: &[u8]) {
    let uid = entry.borrow().uid;
    let Some(body) = lookout_mail::body::parse_body(uid, raw) else {
        show_error(entry, "This message couldn't be displayed.");
        return;
    };
    entry.borrow_mut().last_raw = Some(raw.to_vec());
    render_data(state, entry, &body, Some(raw));
}

/// Renders a parsed body into the window, optionally with the raw bytes
/// available to resolve `cid:` inline images. Applies the external-content
/// policy (with this window's load-once override), rebuilds the attachment
/// strip, and updates the header from the stored summary.
fn render_data(state: &Rc<RefCell<UiState>>, entry: &Rc<RefCell<MessageWindowState>>, body: &EmailBody, raw: Option<&[u8]>) {
    let (
        mailbox,
        uid,
        summary,
        header,
        web_view,
        content_stack,
        text_view,
        remote_banner,
        auth_banner,
        external_banner,
        sensitivity_banner,
        page_stack,
        allow_images,
        allow_all,
        load_images_once,
    ) = {
        let e = entry.borrow();
        (
            e.mailbox.clone(),
            e.uid,
            e.summary.clone(),
            e.header.clone(),
            e.web_view.clone(),
            e.content_stack.clone(),
            e.text_view.clone(),
            e.remote_banner.clone(),
            e.auth_banner.clone(),
            e.external_banner.clone(),
            e.sensitivity_banner.clone(),
            e.page_stack.clone(),
            e.allow_images.clone(),
            e.allow_all.clone(),
            e.load_images_once,
        )
    };
    header.update(&summary);

    // Sender-authentication (6.1), external-sender (6.2) and sensitivity
    // (6.5) info bars, mirroring the reading pane's.
    {
        let mismatch = summary.from.first().and_then(lookout_core::display_name_address_mismatch);
        match crate::window::auth_banner_title(body.auth_results.as_ref(), mismatch.as_deref()) {
            Some(title) => {
                auth_banner.set_title(&title);
                auth_banner.set_revealed(true);
            }
            None => auth_banner.set_revealed(false),
        }
        let domains = state.borrow().settings.get_strv(crate::settings::MAIL_INTERNAL_DOMAINS);
        let external = summary.from.first().is_some_and(|a| lookout_core::is_external_sender(&a.address, &domains));
        external_banner.set_revealed(external);
        match lookout_core::sensitivity_from_headers(&body.headers).unwrap_or_default().as_header_value() {
            Some(label) => {
                sensitivity_banner.set_title(&format!("Sensitivity: {label}"));
                sensitivity_banner.set_revealed(true);
            }
            None => sensitivity_banner.set_revealed(false),
        }
    }

    // The reading pane's attachment strip, reused verbatim: its Open/Save
    // actions ask the account for the part's bytes; the completed fetch is
    // accepted because `message_is_visible` treats this window as on screen.
    crate::window::rebuild_attachment_strip(state, &page_stack, &mailbox, uid, &body.parts);

    let trust_sender = crate::window::mailbox_account_id(&mailbox).zip(summary.from.first().map(|a| a.address.trim().to_lowercase()));
    let (images_allowed, all_allowed) = crate::window::remote_content_policy(state, trust_sender.as_ref(), load_images_once);
    allow_images.set(images_allowed);
    allow_all.set(all_allowed);

    if let Some(html) = &body.html_body {
        let rewritten = match raw {
            Some(raw) => lookout_mail::body::rewrite_cid_refs_to_data_uris(html, raw),
            None => html.clone(),
        };
        let scan = lookout_core::html_remote_content_scan(html);
        let remote_blocked = (scan.has_images && !images_allowed) || (scan.has_other && !all_allowed);
        let loaded = crate::window::wrap_message_with_csp(&rewritten, images_allowed, all_allowed);
        web_view.load_html(&loaded, None);
        content_stack.set_visible_child_name("html");
        // The load-changed handler reveals the page once WebKit finishes
        // painting; this backstop keeps a slow/odd load from stranding the
        // window on its spinner (same role as the reading pane's reveal
        // timeout).
        let page_stack_for_timeout = page_stack.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            if page_stack_for_timeout.visible_child_name().as_deref() == Some("loading") {
                page_stack_for_timeout.set_visible_child_name("message");
            }
        });
        let title = match trust_sender.as_ref() {
            Some((_, sender)) => format!("Remote content from {sender} is blocked"),
            None => "Remote content is blocked".to_string(),
        };
        remote_banner.set_title(&title);
        remote_banner.set_revealed(remote_blocked);
    } else {
        // Plain text (or no body at all): no remote content to gate.
        let text = body.text_body.clone().unwrap_or_default();
        text_view.buffer().set_text(&text);
        content_stack.set_visible_child_name("text");
        remote_banner.set_revealed(false);
        page_stack.set_visible_child_name("message");
    }

    {
        let mut e = entry.borrow_mut();
        e.last_body = Some(body.clone());
        e.rendered = true;
    }
}

/// Wires one reply-family button to the composer, opening it straight into
/// its own window (popped out) prefilled from *this window's* message - the
/// message is already in a standalone window, so its reply should be too. The
/// composer is embedded in the main reading pane for a frame and then popped
/// out on the next idle, exactly like the invitation "Edit response before
/// sending" flow, so nothing here needs to be surfaced in the main window.
fn wire_reply_button(
    button: &gtk::Button,
    mode: crate::compose::ReplyMode,
    title: &'static str,
    entry: &Rc<RefCell<MessageWindowState>>,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    main_reading_stack: &gtk::Stack,
) {
    let entry = entry.clone();
    let state = state.clone();
    let worker = worker.clone();
    let main_reading_stack = main_reading_stack.clone();
    button.connect_clicked(move |_| {
        let summary = entry.borrow().summary.clone();
        let Some((own_email, cmd_tx)) = crate::window::reply_send_context(&state, &summary) else {
            return;
        };
        let body = entry.borrow().last_body.clone().or_else(|| state.borrow_mut().cached_body(&summary.mailbox, summary.uid));
        let Some(body) = body else { return };
        let prefill = crate::compose::build_reply_prefill(&summary, &body, &own_email, mode);
        let rich_text_default = state.borrow().rich_text_default();
        crate::window::show_composer_in_reading_pane_full(
            &state,
            &worker,
            &main_reading_stack,
            title,
            cmd_tx,
            prefill,
            rich_text_default,
            crate::window::mailbox_account_id(&summary.mailbox),
            Some(crate::config_view::SignatureDefault::Reply),
            None,
            true,
        );
    });
}

/// The Forward sibling of `wire_reply_button`: opens the forward composer
/// straight into its own window too.
fn wire_forward_button(button: &gtk::Button, entry: &Rc<RefCell<MessageWindowState>>, state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, main_reading_stack: &gtk::Stack) {
    let entry = entry.clone();
    let state = state.clone();
    let worker = worker.clone();
    let main_reading_stack = main_reading_stack.clone();
    button.connect_clicked(move |_| {
        let summary = entry.borrow().summary.clone();
        if crate::window::reply_send_context(&state, &summary).is_none() {
            return;
        }
        let body = entry.borrow().last_body.clone().or_else(|| state.borrow_mut().cached_body(&summary.mailbox, summary.uid));
        let Some(body) = body else { return };
        crate::window::forward_message(&state, &worker, &main_reading_stack, &summary, &body, true);
    });
}
