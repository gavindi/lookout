use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, EmailSummary, Mailbox, MailboxId};
use lookout_goa::GoaClient;
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{AccountConfig, EndpointConfig};
use webkit::prelude::*;

use crate::folder_tree::{build_multi_account_tree_model, TreeItem};
use crate::goa_credentials::GoaCredentialProvider;
use crate::worker::Worker;

/// Per-account state the UI needs once an `AccountSession` actor is running:
/// how to send it commands, its identity (for compose "From" and toast
/// labeling), and the last folder list it reported (kept here so the
/// multi-account folder tree can be rebuilt in full from all accounts'
/// latest snapshots whenever any one of them changes).
struct AccountHandle {
    cmd_tx: async_channel::Sender<AccountCommand>,
    email: String,
    display_name: String,
    folders: Vec<Mailbox>,
}

/// Mutable UI-thread state the various signal handlers close over. Plain
/// `Rc<RefCell<_>>` is fine here - GTK is single-threaded, so there's no
/// need for `Arc<Mutex<_>>` on this side of the worker-thread boundary.
struct UiState {
    accounts: HashMap<AccountId, AccountHandle>,
    /// Which account owns the currently-open mailbox - drives command
    /// routing (FetchBody, compose "From") and which account's
    /// `MessagesUpdated` events are allowed to update the message list (a
    /// background IDLE resync on some *other* account must not clobber
    /// whatever the user is currently looking at).
    current_account: Option<AccountId>,
    current_mailbox: Option<MailboxId>,
}

/// Strips `Gtk.Paned`'s default visible grey separator line - the card
/// margins already provide a visual gap between panes (see `card_section`),
/// so a painted handle on top of that just looks like a stray line. The
/// handle keeps a comfortable draggable hit-area (`min-width`/`min-height`);
/// only its painted background/border is removed.
fn install_paned_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "paned > separator {
            background: none;
            border: none;
            box-shadow: none;
            min-width: 12px;
            min-height: 12px;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub fn build_window(app: &adw::Application, worker: Rc<Worker>) -> adw::ApplicationWindow {
    install_paned_css();

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
    open_settings_button.connect_clicked(|_| {
        let _ = std::process::Command::new("gnome-control-center").arg("online-accounts").spawn();
    });
    status_page.set_child(Some(&open_settings_button));

    // --- Folder sidebar: one expanded-by-default group per account ---
    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_factory = gtk::SignalListItemFactory::new();
    folder_factory.connect_setup(|_, list_item| {
        let expander = gtk::TreeExpander::new();
        let label = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        expander.set_child(Some(&label));
        list_item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&expander));
    });
    folder_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else { return };
        let Some(expander) = list_item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        expander.set_list_row(Some(&row));
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let tree_item = boxed.borrow::<TreeItem>();
        let Some(label) = expander.child().and_downcast::<gtk::Label>() else { return };
        match &*tree_item {
            TreeItem::Account(account) => {
                label.set_label(&account.label);
                label.set_css_classes(&["heading"]);
            }
            TreeItem::Folder(node) => {
                let unread = node.mailbox.unread;
                let text = if unread > 0 {
                    format!("{}  ({unread})", node.mailbox.name)
                } else {
                    node.mailbox.name.clone()
                };
                label.set_label(&text);
                label.set_css_classes(&[]);
            }
        }
    });

    let folder_list_view = gtk::ListView::new(Some(folder_selection.clone()), Some(folder_factory));
    let folder_scroller = gtk::ScrolledWindow::builder().child(&folder_list_view).vexpand(true).build();
    let folder_card = card_section(&folder_scroller);

    // --- Message list ---
    let message_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let message_selection = gtk::SingleSelection::new(Some(message_store.clone()));
    let message_factory = gtk::SignalListItemFactory::new();
    message_factory.connect_setup(|_, list_item| {
        let box_ = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(10)
            .build();
        let top_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        let sender = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let date = gtk::Label::builder().xalign(1.0).css_classes(["dim-label", "caption"]).build();
        top_row.append(&sender);
        top_row.append(&date);
        let subject = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        box_.append(&top_row);
        box_.append(&subject);
        list_item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&box_));
    });
    message_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let summary = boxed.borrow::<EmailSummary>();
        let Some(box_) = list_item.child().and_downcast::<gtk::Box>() else { return };
        let Some(top_row) = box_.first_child().and_downcast::<gtk::Box>() else { return };
        let sender = top_row.first_child().and_downcast::<gtk::Label>().unwrap();
        let date_label = top_row.last_child().and_downcast::<gtk::Label>().unwrap();
        let subject = box_.last_child().and_downcast::<gtk::Label>().unwrap();

        let from = summary.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
        sender.set_label(&from);
        sender.set_css_classes(if summary.is_unread() { &["heading"] } else { &[] });
        date_label.set_label(&summary.date.format("%Y-%m-%d %H:%M").to_string());
        subject.set_label(summary.subject.as_deref().unwrap_or("(no subject)"));
    });
    let message_list_view = gtk::ListView::new(Some(message_selection.clone()), Some(message_factory));
    let message_scroller = gtk::ScrolledWindow::builder().child(&message_list_view).vexpand(true).build();
    let message_card = card_section(&message_scroller);
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
    let webkit_settings = webkit::Settings::new();
    webkit_settings.set_enable_javascript(false);
    webkit_settings.set_enable_developer_extras(false);
    let web_view = webkit::WebView::builder().settings(&webkit_settings).hexpand(true).vexpand(true).build();
    // Block navigation *away* from the loaded message body (e.g. clicking a
    // link) - but NOT the initial programmatic `load_html()` call itself,
    // which also fires a NavigationAction decision. Distinguish the two via
    // `is_user_gesture()`: a real click is a user gesture, `load_html()` is
    // not. Getting this wrong (blocking unconditionally) silently vetoes
    // every load, which is exactly the "reading pane always blank" bug this
    // fixes - the WebView was never rendering anything because its own
    // initial content load was being cancelled before it started. External
    // links should ideally open in the system browser instead of just being
    // dropped - full "open externally" handling is a Phase 2 refinement.
    web_view.connect_decide_policy(|_view, decision, decision_type| {
        if decision_type == webkit::PolicyDecisionType::NavigationAction {
            let is_user_gesture = decision
                .downcast_ref::<webkit::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action())
                .map(|action| action.is_user_gesture())
                .unwrap_or(false);
            if is_user_gesture {
                decision.ignore();
                return true;
            }
        }
        false
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

    let reading_stack = gtk::Stack::new();
    reading_stack.add_named(&web_view, Some("html"));
    reading_stack.add_named(&text_scroller, Some("text"));
    let reading_empty = adw::StatusPage::builder().icon_name("mail-message-new-symbolic").title("No Message Selected").build();
    reading_stack.add_named(&reading_empty, Some("empty"));
    reading_stack.set_visible_child_name("empty");
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

    let reading_card = card_section(&reading_stack);

    // --- Resizable panes: folders | (messages | reading), each its own
    // rounded card. `resize_start_child(false)` keeps the sidebar-like
    // panes from silently growing when the window resizes; the reading
    // pane absorbs the extra space.
    let messages_reading_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&message_card)
        .end_child(&reading_card)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(320)
        .build();
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

    let status_page_as_widget: gtk::Widget = status_page.clone().upcast();
    let main_paned_as_widget: gtk::Widget = main_paned.clone().upcast();
    let root_stack = gtk::Stack::new();
    root_stack.add_named(&status_page_as_widget, Some("empty"));
    root_stack.add_named(&main_paned_as_widget, Some("mail"));
    root_stack.set_visible_child_name("empty");

    // The one real title bar for the window - owns the actual
    // minimize/maximize/close buttons. The per-card header bars inside
    // `root_stack` are explicitly told not to show these (see
    // `card_section`), so there's exactly one set, not four.
    let window_header = adw::HeaderBar::new();
    window_header.set_title_widget(Some(&adw::WindowTitle::new("Lookout", "")));
    window_header.pack_end(&compose_button);
    #[cfg(debug_assertions)]
    window_header.pack_end(&open_eml_button);
    let outer_toolbar_view = adw::ToolbarView::new();
    outer_toolbar_view.add_top_bar(&window_header);
    outer_toolbar_view.set_content(Some(&root_stack));

    toast_overlay.set_child(Some(&outer_toolbar_view));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lookout")
        .default_width(1600)
        .default_height(900)
        .content(&toast_overlay)
        .build();

    let state = Rc::new(RefCell::new(UiState {
        accounts: HashMap::new(),
        current_account: None,
        current_mailbox: None,
    }));

    // --- Compose button -> new-message window, "From" = the account owning
    // the currently-open mailbox (falling back to any connected account if
    // nothing's been selected yet) ---
    {
        let state = state.clone();
        let window = window.clone();
        compose_button.connect_clicked(move |_| {
            let st = state.borrow();
            let account_id = st.current_account.clone().or_else(|| st.accounts.keys().next().cloned());
            let Some(handle) = account_id.and_then(|id| st.accounts.get(&id)) else { return };
            let cmd_tx = handle.cmd_tx.clone();
            let from_email = handle.email.clone();
            drop(st);
            crate::compose::open_compose_window(&window, from_email, cmd_tx, None, None, None);
        });
    }

    // --- Debug: open a raw .eml fixture straight into the reading pane ---
    #[cfg(debug_assertions)]
    {
        let window = window.clone();
        let reading_stack = reading_stack.clone();
        open_eml_button.connect_clicked(move |_| {
            let window = window.clone();
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
                let Ok(raw) = std::fs::read(&path) else { return };
                if let Some(body) = lookout_mail::body::parse_body(lookout_core::Uid(0), &raw) {
                    render_body(&reading_stack, body);
                }
            });
        });
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
    // own account (selecting an account-group row itself is a no-op - it
    // just expands/collapses) ---
    {
        let state = state.clone();
        folder_selection.connect_selected_item_notify(move |sel| {
            let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let tree_item = boxed.borrow::<TreeItem>();
            let TreeItem::Folder(node) = &*tree_item else { return };
            let mailbox_id = node.mailbox.id.clone();
            let account_id = node.mailbox.account_id.clone();
            drop(tree_item);

            let mut st = state.borrow_mut();
            st.current_account = Some(account_id.clone());
            st.current_mailbox = Some(mailbox_id.clone());
            if let Some(handle) = st.accounts.get(&account_id) {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::SyncMailbox(mailbox_id));
            }
        });
    }

    // --- Message selection -> AccountCommand::FetchBody on the current account ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        message_selection.connect_selected_item_notify(move |sel| {
            let Some(boxed) = sel.selected_item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let summary = boxed.borrow::<EmailSummary>();
            let uid = summary.uid;
            let mailbox = summary.mailbox.clone();
            drop(summary);

            let st = state.borrow();
            if let Some(handle) = st.current_account.as_ref().and_then(|id| st.accounts.get(id)) {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::FetchBody { mailbox, uid });
            }
            drop(st);
            reading_stack.set_visible_child_name("empty");
        });
    }

    spawn_account_discovery(worker, state, root_stack, toast_overlay.clone(), folder_selection, message_store, reading_stack);

    window
}

fn spawn_account_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    folder_selection: gtk::SingleSelection,
    message_store: gio::ListStore,
    reading_stack: gtk::Stack,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_mail_accounts().await?;
            Ok::<_, lookout_goa::Error>((client, accounts))
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        match result {
            Ok((client, accounts)) if !accounts.is_empty() => {
                root_stack.set_visible_child_name("mail");
                // One AccountSession actor per connected account, all
                // running concurrently on the worker thread. `GoaClient` is
                // a cheap Arc-backed handle (see its doc comment), so
                // cloning it per account reuses the one D-Bus connection
                // rather than opening a redundant one each time.
                for account in accounts {
                    connect_account(
                        worker.clone(),
                        state.clone(),
                        folder_selection.clone(),
                        message_store.clone(),
                        reading_stack.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        account,
                    );
                }
            }
            Ok(_) => {
                root_stack.set_visible_child_name("empty");
            }
            Err(e) => {
                root_stack.set_visible_child_name("empty");
                toast_overlay.add_toast(adw::Toast::new(&format!("Couldn't reach GNOME Online Accounts: {e}")));
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn connect_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    folder_selection: gtk::SingleSelection,
    message_store: gio::ListStore,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
    account: lookout_goa::GoaMailAccount,
) {
    let account_id = AccountId(account.account_id.0.clone());
    let display_name = account.display_name.clone();
    let config = AccountConfig {
        account_id: account_id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        imap: EndpointConfig {
            host: account.imap.host.clone(),
            port: account.imap.port.unwrap_or(993),
            use_tls: account.imap.use_tls,
            username: account.imap.username.clone(),
        },
        smtp: EndpointConfig {
            host: account.smtp.host.clone(),
            port: account.smtp.port.unwrap_or(587),
            use_tls: account.smtp.use_tls,
            username: account.smtp.username.clone(),
        },
    };
    let credentials: Rc<dyn lookout_mail::session::CredentialProvider> = Rc::new(GoaCredentialProvider::new(goa_client, account));
    // `run_account_session` requires `Arc<dyn CredentialProvider>` (it may
    // run reconnect attempts on the worker thread's own async tasks), so
    // wrap in a thread-safe handle even though only ever driven from one
    // worker task at a time.
    struct SendWrapper(Rc<dyn lookout_mail::session::CredentialProvider>);
    // SAFETY: the wrapped provider only ever runs on the single worker
    // thread's tokio tasks for this account; it is never touched from the
    // UI thread after being handed off here.
    unsafe impl Send for SendWrapper {}
    unsafe impl Sync for SendWrapper {}
    #[async_trait::async_trait]
    impl lookout_mail::session::CredentialProvider for SendWrapper {
        async fn imap_credential(&self) -> Result<lookout_mail::Credential, String> {
            self.0.imap_credential().await
        }

        async fn smtp_credential(&self) -> Result<lookout_mail::Credential, String> {
            self.0.smtp_credential().await
        }
    }
    let credentials: std::sync::Arc<dyn lookout_mail::session::CredentialProvider> = std::sync::Arc::new(SendWrapper(credentials));

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.insert(
        account_id.clone(),
        AccountHandle {
            cmd_tx,
            email: config.email.clone(),
            display_name,
            folders: Vec::new(),
        },
    );

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            match event {
                AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, .. }) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", account_label(&state, &account_id))));
                }
                AccountEvent::ConnectionStateChanged(_) => {}
                AccountEvent::FoldersUpdated(folders) => {
                    if let Some(handle) = state.borrow_mut().accounts.get_mut(&account_id) {
                        handle.folders = folders;
                    }
                    rebuild_folder_tree(&state.borrow(), &folder_selection);
                }
                AccountEvent::MessagesUpdated { mailbox, messages } => {
                    let should_display = {
                        let mut st = state.borrow_mut();
                        // Nothing selected yet (fresh startup): adopt
                        // whichever account's initial inbox sync lands
                        // first as the default view, rather than leaving
                        // the message list empty until the user clicks a
                        // folder. Whichever connected account happens to
                        // finish its first sync first wins this race -
                        // an acceptable, benign nondeterminism for Phase 1.
                        if st.current_mailbox.is_none() {
                            st.current_account = Some(account_id.clone());
                            st.current_mailbox = Some(mailbox.clone());
                        }
                        st.current_mailbox.as_ref() == Some(&mailbox)
                    };
                    if should_display {
                        message_store.remove_all();
                        // Newest first for the reading list.
                        let mut messages = messages;
                        messages.sort_by_key(|m| std::cmp::Reverse(m.date));
                        for m in messages {
                            message_store.append(&glib::BoxedAnyObject::new(m));
                        }
                    }
                }
                AccountEvent::BodyFetched { body, .. } => {
                    render_body(&reading_stack, body);
                }
                AccountEvent::SendCompleted => {
                    toast_overlay.add_toast(adw::Toast::new("Message sent"));
                }
                AccountEvent::Error(message) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", account_label(&state, &account_id))));
                }
            }
        }
    });
}

fn account_label(state: &Rc<RefCell<UiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

/// Rebuilds the folder sidebar's `Gtk.TreeListModel` from every connected
/// account's latest folder snapshot. Accounts are sorted by email for a
/// stable order across rebuilds (`HashMap` iteration order isn't stable,
/// and accounts visibly reshuffling on every resync would be jarring).
fn rebuild_folder_tree(state: &UiState, folder_selection: &gtk::SingleSelection) {
    let mut accounts: Vec<(AccountId, String, Vec<Mailbox>)> = state
        .accounts
        .iter()
        .map(|(id, handle)| {
            let label = if handle.display_name.is_empty() {
                handle.email.clone()
            } else {
                handle.display_name.clone()
            };
            (id.clone(), label, handle.folders.clone())
        })
        .collect();
    accounts.sort_by_key(|a| a.1.to_lowercase());

    let model = build_multi_account_tree_model(accounts);
    folder_selection.set_model(Some(&model));
}

/// Wraps `content` directly in a `.card`-styled box - libadwaita's real,
/// built-in style class for a rounded-corner, softly-shaded panel. No
/// per-section header bar: `Gtk.Paned` (see `build_window`) provides the
/// resize handle and visual gap between cards, and the small margin here is
/// what makes each card's rounded corners actually visible against the
/// window background instead of touching the divider/edges.
fn card_section(content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"])
        .overflow(gtk::Overflow::Hidden)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    card.append(content);
    card
}

fn render_body(reading_stack: &gtk::Stack, body: lookout_core::EmailBody) {
    if let Some(html) = &body.html_body {
        if let Some(web_view) = reading_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
            web_view.load_html(html, None);
            reading_stack.set_visible_child_name("html");
            return;
        }
    }
    if let Some(text) = &body.text_body {
        if let Some(scroller) = reading_stack.child_by_name("text").and_downcast::<gtk::ScrolledWindow>() {
            if let Some(text_view) = scroller.child().and_downcast::<gtk::TextView>() {
                text_view.buffer().set_text(text);
                reading_stack.set_visible_child_name("text");
                return;
            }
        }
    }
    reading_stack.set_visible_child_name("empty");
}
