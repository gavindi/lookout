use adw::prelude::*;
use lookout_core::{EmailBody, EmailSummary};
use lookout_mail::session::AccountCommand;
use lookout_mail::ComposedMessage;

/// Everything the compose window can be pre-filled with, beyond a blank "New
/// Message" (`ComposePrefill::default()`). Grouped into one struct rather
/// than more loose parameters since Reply/Reply-All/Forward all need to set
/// several of these fields together.
#[derive(Default)]
pub struct ComposePrefill {
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    /// RFC 5322 `In-Reply-To` - the bare Message-Id (no `<>`) of the message
    /// being replied to. `mail_builder`'s `MessageId` header writer adds the
    /// angle brackets itself, so this must NOT include them.
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References` - bare Message-Ids (no `<>`), oldest first.
    pub references: Vec<String>,
}

/// Whether Reply excludes the original's other recipients (`Reply`) or
/// carries them all forward minus the replying account itself (`ReplyAll`).
#[derive(Clone, Copy)]
pub enum ReplyMode {
    Reply,
    ReplyAll,
}

/// Case-insensitive lookup against `EmailBody::headers` (raw RFC 5322
/// headers, verbatim - see that field's doc comment). IMAP's `ENVELOPE`
/// doesn't carry `References`, so this is the only place that header is
/// available at all; `Message-Id` is looked up the same way for consistency.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Strips one layer of RFC 5322 `<...>` angle brackets from a Message-Id
/// token, if present. `mail_builder`'s `MessageId` header writer adds its
/// own brackets around each id it's given, so a raw header value (which
/// already has them) must be stripped first or the sent header would come
/// out double-bracketed.
fn strip_angle_brackets(id: &str) -> &str {
    id.trim().trim_start_matches('<').trim_end_matches('>')
}

fn own_address_matches(address: &str, own_email: &str) -> bool {
    address.eq_ignore_ascii_case(own_email)
}

fn with_prefix_once(subject: &str, prefix: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
        trimmed.to_string()
    } else {
        format!("{prefix}{trimmed}")
    }
}

fn quote_lines(text: &str) -> String {
    text.lines().map(|line| format!("> {line}")).collect::<Vec<_>>().join("\n")
}

fn sender_label(summary: &EmailSummary) -> String {
    summary.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "someone".to_string())
}

/// Builds the `in_reply_to`/`references` chain for a reply from the
/// original message's raw headers (never from `EmailSummary::references`,
/// which is always empty - see `header_value`'s doc comment).
fn reply_threading(body: &EmailBody) -> (Option<String>, Vec<String>) {
    let original_id = header_value(&body.headers, "message-id").map(strip_angle_brackets);
    let mut references: Vec<String> = header_value(&body.headers, "references")
        .map(|v| v.split_whitespace().map(strip_angle_brackets).map(str::to_string).collect())
        .unwrap_or_default();
    if let Some(id) = original_id {
        references.push(id.to_string());
    }
    (original_id.map(str::to_string), references)
}

/// Builds the Reply/Reply-All prefill: recipients (bare addresses, own
/// address excluded), a `Re: `-prefixed subject, the quoted original body,
/// and the Message-Id/References chain so the reply threads correctly.
pub fn build_reply_prefill(summary: &EmailSummary, body: &EmailBody, own_email: &str, mode: ReplyMode) -> ComposePrefill {
    let mut to: Vec<String> = summary.from.iter().map(|a| a.address.clone()).collect();
    let mut cc: Vec<String> = Vec::new();
    if matches!(mode, ReplyMode::ReplyAll) {
        to.extend(summary.to.iter().map(|a| a.address.clone()));
        cc.extend(summary.cc.iter().map(|a| a.address.clone()));
    }
    to.retain(|a| !own_address_matches(a, own_email));
    cc.retain(|a| !own_address_matches(a, own_email));

    let subject = summary.subject.as_deref().map(|s| with_prefix_once(s, "Re: ")).unwrap_or_else(|| "Re: ".to_string());

    let quoted = body.text_body.as_deref().unwrap_or("");
    let reply_body = format!("\n\nOn {}, {} wrote:\n{}", summary.date.format("%a, %b %d, %Y at %I:%M %p"), sender_label(summary), quote_lines(quoted));

    let (in_reply_to, references) = reply_threading(body);

    ComposePrefill {
        to: Some(to.join(", ")),
        cc: if cc.is_empty() { None } else { Some(cc.join(", ")) },
        subject: Some(subject),
        body: Some(reply_body),
        in_reply_to,
        references,
    }
}

/// Builds the Forward prefill: blank recipients (the user fills these in), a
/// `Fwd: `-prefixed subject, and a forwarded-message header block followed
/// by the original body verbatim. Unlike Reply, this starts a new,
/// unthreaded conversation - no `in_reply_to`/`references` are set.
pub fn build_forward_prefill(summary: &EmailSummary, body: &EmailBody) -> ComposePrefill {
    let subject = summary.subject.as_deref().map(|s| with_prefix_once(s, "Fwd: ")).unwrap_or_else(|| "Fwd: ".to_string());

    let from = summary.from.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let to = summary.to.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let original = body.text_body.as_deref().unwrap_or("");
    let forward_body = format!(
        "\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n{}",
        from,
        summary.date.format("%a, %b %d, %Y at %I:%M %p"),
        summary.subject.as_deref().unwrap_or(""),
        to,
        original
    );

    ComposePrefill {
        to: None,
        cc: None,
        subject: Some(subject),
        body: Some(forward_body),
        in_reply_to: None,
        references: Vec::new(),
    }
}

/// Opens a plain-text compose window. Rich-text/contenteditable compose was
/// the plan's own flagged highest-risk Phase 1 item; this ships the
/// documented fallback (plain `Gtk.TextView` body, comma-separated
/// recipients) so sending mail works end-to-end now, with a richer composer
/// as Phase 2 work.
pub fn open_compose_window(parent: &adw::ApplicationWindow, from_email: String, cmd_tx: async_channel::Sender<AccountCommand>, prefill: ComposePrefill) {
    let to_row = adw::EntryRow::builder().title("To").build();
    if let Some(to) = &prefill.to {
        to_row.set_text(to);
    }
    let cc_row = adw::EntryRow::builder().title("Cc").build();
    if let Some(cc) = &prefill.cc {
        cc_row.set_text(cc);
    }
    let subject_row = adw::EntryRow::builder().title("Subject").build();
    if let Some(subject) = &prefill.subject {
        subject_row.set_text(subject);
    }

    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(&to_row);
    fields_group.add(&cc_row);
    fields_group.add(&subject_row);

    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    if let Some(body) = &prefill.body {
        body_view.buffer().set_text(body);
    }
    let body_scroller = gtk::ScrolledWindow::builder().child(&body_view).vexpand(true).build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
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
        let in_reply_to = prefill.in_reply_to;
        let references = prefill.references;
        send_button.connect_clicked(move |_| {
            let to: Vec<String> = to_row.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if to.is_empty() {
                return;
            }
            let cc: Vec<String> = cc_row.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let buffer = body_view.buffer();
            let text_body = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
            let msg = ComposedMessage {
                from: from_email.clone(),
                to,
                cc,
                bcc: Vec::new(),
                subject: subject_row.text().to_string(),
                text_body,
                in_reply_to: in_reply_to.clone(),
                references: references.clone(),
            };
            let _ = cmd_tx.send_blocking(AccountCommand::SendMessage(msg));
            window.close();
        });
    }

    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{AccountId, BodyPart, EmailAddress, MailboxId, ThreadKey, Uid};
    use std::collections::BTreeSet;

    fn sample_summary() -> EmailSummary {
        let account_id = AccountId("/test/account".to_string());
        EmailSummary {
            uid: Uid(1),
            mailbox: MailboxId::new(&account_id, "INBOX"),
            message_id: Some("orig@example.com".to_string()),
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey(String::new()),
            subject: Some("Hello there".to_string()),
            from: vec![EmailAddress { name: Some("Ada Lovelace".to_string()), address: "ada@example.com".to_string() }],
            to: vec![EmailAddress { name: None, address: "me@example.com".to_string() }, EmailAddress { name: None, address: "other@example.com".to_string() }],
            cc: vec![EmailAddress { name: None, address: "cc-person@example.com".to_string() }],
            date: Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap(),
            flags: BTreeSet::new(),
            keywords: BTreeSet::new(),
            size: 100,
            has_attachment: false,
            preview: None,
        }
    }

    fn sample_body(headers: Vec<(String, String)>, text_body: Option<&str>) -> EmailBody {
        EmailBody {
            uid: Uid(1),
            text_body: text_body.map(str::to_string),
            html_body: None,
            parts: Vec::<BodyPart>::new(),
            headers,
            auth_results: None,
        }
    }

    #[test]
    fn header_value_matches_case_insensitively() {
        let headers = vec![("Message-ID".to_string(), "<abc@example.com>".to_string())];
        assert_eq!(header_value(&headers, "message-id"), Some("<abc@example.com>"));
        assert_eq!(header_value(&headers, "Message-Id"), Some("<abc@example.com>"));
        assert_eq!(header_value(&headers, "subject"), None);
    }

    #[test]
    fn subject_prefix_is_not_doubled_when_already_present() {
        assert_eq!(with_prefix_once("Hello", "Re: "), "Re: Hello");
        assert_eq!(with_prefix_once("Re: Hello", "Re: "), "Re: Hello");
        assert_eq!(with_prefix_once("re: Hello", "Re: "), "re: Hello");
    }

    #[test]
    fn reply_threading_strips_angle_brackets_and_appends_original_id() {
        let headers = vec![
            ("Message-ID".to_string(), "<orig@example.com>".to_string()),
            ("References".to_string(), "<older@example.com> <older2@example.com>".to_string()),
        ];
        let body = sample_body(headers, Some("hi"));
        let (in_reply_to, references) = reply_threading(&body);
        assert_eq!(in_reply_to, Some("orig@example.com".to_string()));
        assert_eq!(references, vec!["older@example.com".to_string(), "older2@example.com".to_string(), "orig@example.com".to_string()]);
    }

    #[test]
    fn reply_threading_with_no_references_header_falls_back_to_just_message_id() {
        let headers = vec![("Message-ID".to_string(), "<orig@example.com>".to_string())];
        let body = sample_body(headers, Some("hi"));
        let (in_reply_to, references) = reply_threading(&body);
        assert_eq!(in_reply_to, Some("orig@example.com".to_string()));
        assert_eq!(references, vec!["orig@example.com".to_string()]);
    }

    #[test]
    fn reply_excludes_other_recipients_reply_all_includes_them_minus_own_address() {
        let summary = sample_summary();
        let body = sample_body(vec![("Message-ID".to_string(), "<orig@example.com>".to_string())], Some("original text"));

        let reply = build_reply_prefill(&summary, &body, "me@example.com", ReplyMode::Reply);
        assert_eq!(reply.to.as_deref(), Some("ada@example.com"));
        assert_eq!(reply.cc, None);

        let reply_all = build_reply_prefill(&summary, &body, "me@example.com", ReplyMode::ReplyAll);
        assert_eq!(reply_all.to.as_deref(), Some("ada@example.com, other@example.com"));
        assert_eq!(reply_all.cc.as_deref(), Some("cc-person@example.com"));
    }

    #[test]
    fn forward_prefill_has_no_threading_and_blank_recipients() {
        let summary = sample_summary();
        let body = sample_body(vec![("Message-ID".to_string(), "<orig@example.com>".to_string())], Some("original text"));
        let forward = build_forward_prefill(&summary, &body);
        assert_eq!(forward.to, None);
        assert_eq!(forward.cc, None);
        assert_eq!(forward.in_reply_to, None);
        assert!(forward.references.is_empty());
        assert_eq!(forward.subject.as_deref(), Some("Fwd: Hello there"));
        assert!(forward.body.unwrap().contains("original text"));
    }
}
