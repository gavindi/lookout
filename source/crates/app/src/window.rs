use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use webkit::prelude::*;
use lookout_core::{AccountId, EmailSummary, MailboxId};
use lookout_goa::GoaClient;
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{AccountConfig, EndpointConfig};

use crate::folder_tree::{build_tree_model, FolderNode};
use crate::goa_credentials::GoaCredentialProvider;
use crate::worker::Worker;

/// Mutable UI-thread state the various signal handlers close over. Plain
/// `Rc<RefCell<_>>` is fine here - GTK is single-threaded, so there's no
/// need for `Arc<Mutex<_>>` on this side of the worker-thread boundary.
struct UiState {
    cmd_tx: Option<async_channel::Sender<AccountCommand>>,
    current_mailbox: Option<MailboxId>,
    account_id: Option<AccountId>,
    from_email: Option<String>,
}

pub fn build_window(app: &adw::Application, worker: Rc<Worker>) -> adw::ApplicationWindow {
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

    // --- Folder sidebar ---
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
        let Some(expander) = list_item.child().and_downcast::<gtk::TreeExpander>() else { return };
        expander.set_list_row(Some(&row));
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let node = boxed.borrow::<Rc<FolderNode>>();
        if let Some(label) = expander.child().and_downcast::<gtk::Label>() {
            let unread = node.mailbox.unread;
            let text =
                if unread > 0 { format!("{}  ({unread})", node.mailbox.name) } else { node.mailbox.name.clone() };
            label.set_label(&text);
        }
    });

    let folder_list_view = gtk::ListView::new(Some(folder_selection.clone()), Some(folder_factory));
    let folder_scroller = gtk::ScrolledWindow::builder().child(&folder_list_view).vexpand(true).build();
    let folder_page = adw::NavigationPage::builder()
        .title("Folders")
        .child(&adw::ToolbarView::builder().top_bar_style(adw::ToolbarStyle::Flat).content(&folder_scroller).build())
        .build();
    // Placeholder header bar so the folder page has its own titlebar even
    // before AdwNavigationSplitView wraps it.
    {
        let toolbar_view = folder_page.child().and_downcast::<adw::ToolbarView>().unwrap();
        toolbar_view.add_top_bar(&adw::HeaderBar::new());
    }

    // --- Message list ---
    let message_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let message_selection = gtk::SingleSelection::new(Some(message_store.clone()));
    let message_factory = gtk::SignalListItemFactory::new();
    message_factory.connect_setup(|_, list_item| {
        let box_ = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).margin_top(6).margin_bottom(6).margin_start(10).margin_end(10).build();
        let top_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        let sender = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let date = gtk::Label::builder().xalign(1.0).css_classes(["dim-label", "caption"]).build();
        top_row.append(&sender);
        top_row.append(&date);
        let subject = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["dim-label"]).build();
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
    let message_page = adw::NavigationPage::builder()
        .title("Messages")
        .child(&adw::ToolbarView::builder().top_bar_style(adw::ToolbarStyle::Flat).content(&message_scroller).build())
        .build();
    let compose_button = gtk::Button::from_icon_name("mail-message-new-symbolic");
    compose_button.set_tooltip_text(Some("New Message"));
    {
        let header = adw::HeaderBar::new();
        header.pack_end(&compose_button);
        let toolbar_view = message_page.child().and_downcast::<adw::ToolbarView>().unwrap();
        toolbar_view.add_top_bar(&header);
    }

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

    let text_view = gtk::TextView::builder().editable(false).cursor_visible(false).wrap_mode(gtk::WrapMode::WordChar).left_margin(12).right_margin(12).top_margin(12).bottom_margin(12).build();
    let text_scroller = gtk::ScrolledWindow::builder().child(&text_view).build();

    let reading_stack = gtk::Stack::new();
    reading_stack.add_named(&web_view, Some("html"));
    reading_stack.add_named(&text_scroller, Some("text"));
    let reading_empty = adw::StatusPage::builder().icon_name("mail-message-new-symbolic").title("No Message Selected").build();
    reading_stack.add_named(&reading_empty, Some("empty"));
    reading_stack.set_visible_child_name("empty");

    let reading_page = adw::NavigationPage::builder()
        .title("Message")
        .child(&adw::ToolbarView::builder().top_bar_style(adw::ToolbarStyle::Flat).content(&reading_stack).build())
        .build();
    {
        let toolbar_view = reading_page.child().and_downcast::<adw::ToolbarView>().unwrap();
        toolbar_view.add_top_bar(&adw::HeaderBar::new());
    }

    // --- Nested split views: folders | (messages | reading) ---
    let inner_split = adw::NavigationSplitView::builder().sidebar(&message_page).content(&reading_page).build();
    let inner_split_page = adw::NavigationPage::builder().title("Mail").child(&inner_split).build();
    let outer_split = adw::NavigationSplitView::builder().sidebar(&folder_page).content(&inner_split_page).build();

    let status_page_as_widget: gtk::Widget = status_page.clone().upcast();
    let outer_split_as_widget: gtk::Widget = outer_split.clone().upcast();
    let root_stack = gtk::Stack::new();
    root_stack.add_named(&status_page_as_widget, Some("empty"));
    root_stack.add_named(&outer_split_as_widget, Some("mail"));
    root_stack.set_visible_child_name("empty");

    toast_overlay.set_child(Some(&root_stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lookout")
        .default_width(1100)
        .default_height(720)
        .content(&toast_overlay)
        .build();

    let state = Rc::new(RefCell::new(UiState { cmd_tx: None, current_mailbox: None, account_id: None, from_email: None }));

    // --- Compose button -> new-message window ---
    {
        let state = state.clone();
        let window = window.clone();
        compose_button.connect_clicked(move |_| {
            let st = state.borrow();
            let (Some(cmd_tx), Some(from_email)) = (st.cmd_tx.clone(), st.from_email.clone()) else { return };
            drop(st);
            crate::compose::open_compose_window(&window, from_email, cmd_tx, None, None, None);
        });
    }

    // --- Network connectivity -> nudge a backed-off session to retry now ---
    {
        let state = state.clone();
        let monitor = gio::NetworkMonitor::default();
        monitor.connect_network_changed(move |_monitor, available| {
            if !available {
                return;
            }
            let st = state.borrow();
            if let Some(tx) = &st.cmd_tx {
                let _ = tx.send_blocking(AccountCommand::Reconnect);
            }
        });
    }

    // --- Folder selection -> AccountCommand::SyncMailbox ---
    {
        let state = state.clone();
        folder_selection.connect_selected_item_notify(move |sel| {
            let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let node = boxed.borrow::<Rc<FolderNode>>();
            let mailbox_id = node.mailbox.id.clone();
            let st = state.borrow();
            if let Some(tx) = &st.cmd_tx {
                let _ = tx.send_blocking(AccountCommand::SyncMailbox(mailbox_id));
            }
        });
    }

    // --- Message selection -> AccountCommand::FetchBody ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        message_selection.connect_selected_item_notify(move |sel| {
            let Some(boxed) = sel.selected_item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let summary = boxed.borrow::<EmailSummary>();
            let uid = summary.uid;
            let mailbox = summary.mailbox.clone();
            drop(summary);
            let st = state.borrow();
            if let Some(tx) = &st.cmd_tx {
                let _ = tx.send_blocking(AccountCommand::FetchBody { mailbox, uid });
            }
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
            Ok((client, mut accounts)) if !accounts.is_empty() => {
                let account = accounts.remove(0);
                root_stack.set_visible_child_name("mail");
                connect_account(worker, state, folder_selection, message_store, reading_stack, toast_overlay, client, account);
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
    let credentials: Rc<dyn lookout_mail::session::CredentialProvider> =
        Rc::new(GoaCredentialProvider::new(goa_client, account));
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
    let credentials: std::sync::Arc<dyn lookout_mail::session::CredentialProvider> =
        std::sync::Arc::new(SendWrapper(credentials));

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    state.borrow_mut().cmd_tx = Some(cmd_tx);
    state.borrow_mut().account_id = Some(account_id);
    state.borrow_mut().from_email = Some(config.email.clone());

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            match event {
                AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, .. }) => {
                    toast_overlay.add_toast(adw::Toast::new(&message));
                }
                AccountEvent::ConnectionStateChanged(_) => {}
                AccountEvent::FoldersUpdated(folders) => {
                    let Some(account_id) = state.borrow().account_id.clone() else { continue };
                    let model = build_tree_model(folders, &account_id);
                    folder_selection.set_model(Some(&model));
                }
                AccountEvent::MessagesUpdated { mailbox, messages } => {
                    state.borrow_mut().current_mailbox = Some(mailbox);
                    message_store.remove_all();
                    // Newest first for the reading list.
                    let mut messages = messages;
                    messages.sort_by_key(|m| std::cmp::Reverse(m.date));
                    for m in messages {
                        message_store.append(&glib::BoxedAnyObject::new(m));
                    }
                }
                AccountEvent::BodyFetched { body, .. } => {
                    render_body(&reading_stack, body);
                }
                AccountEvent::SendCompleted => {
                    toast_overlay.add_toast(adw::Toast::new("Message sent"));
                }
                AccountEvent::Error(message) => {
                    toast_overlay.add_toast(adw::Toast::new(&message));
                }
            }
        }
    });
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
