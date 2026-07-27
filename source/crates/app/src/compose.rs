use adw::prelude::*;
use lookout_mail::session::AccountCommand;
use lookout_mail::ComposedMessage;

/// Opens a plain-text compose window. Rich-text/contenteditable compose was
/// the plan's own flagged highest-risk Phase 1 item; this ships the
/// documented fallback (plain `Gtk.TextView` body, comma-separated
/// recipients) so sending mail works end-to-end now, with a richer composer
/// as Phase 2 work.
pub fn open_compose_window(
    parent: &adw::ApplicationWindow,
    from_email: String,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill_to: Option<String>,
    prefill_subject: Option<String>,
    prefill_body: Option<String>,
) {
    let to_row = adw::EntryRow::builder().title("To").build();
    if let Some(to) = &prefill_to {
        to_row.set_text(to);
    }
    let subject_row = adw::EntryRow::builder().title("Subject").build();
    if let Some(subject) = &prefill_subject {
        subject_row.set_text(subject);
    }

    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(&to_row);
    fields_group.add(&subject_row);

    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    if let Some(body) = &prefill_body {
        body_view.buffer().set_text(body);
    }
    let body_scroller = gtk::ScrolledWindow::builder().child(&body_view).vexpand(true).build();

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(12).margin_start(12).margin_end(12).build();
    content.append(&fields_group);
    content.append(&body_scroller);

    let cancel_button = gtk::Button::builder().label("Cancel").build();
    let send_button = gtk::Button::builder().label("Send").css_classes(["suggested-action"]).build();

    let header = adw::HeaderBar::builder().show_end_title_buttons(false).show_start_title_buttons(false).build();
    header.pack_start(&cancel_button);
    header.pack_end(&send_button);
    header.set_title_widget(Some(&gtk::Label::new(Some("New Message"))));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&content));

    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(false)
        .default_width(640)
        .default_height(480)
        .title("New Message")
        .content(&toolbar_view)
        .build();

    {
        let window = window.clone();
        cancel_button.connect_clicked(move |_| window.close());
    }
    {
        let window = window.clone();
        send_button.connect_clicked(move |_| {
            let to: Vec<String> = to_row.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if to.is_empty() {
                return;
            }
            let buffer = body_view.buffer();
            let text_body = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
            let msg = ComposedMessage {
                from: from_email.clone(),
                to,
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: subject_row.text().to_string(),
                text_body,
                in_reply_to: None,
                references: Vec::new(),
            };
            let _ = cmd_tx.send_blocking(AccountCommand::SendMessage(msg));
            window.close();
        });
    }

    window.present();
}
