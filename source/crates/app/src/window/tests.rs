use super::*;
use crate::folder_tree::build_multi_account_tree_model;
use crate::google_tasks::GoogleTasksEvent;
use lookout_core::calendar_protocol::CalendarSessionEvent;
use lookout_core::{BodyPart, CalendarId, EmailAddress, EventUid, UidValidity, VCard};
use lookout_dav::subscription::SubscriptionSessionEvent;
use lookout_mail::session::AccountEvent;

// --- Outbox banner (item 1.14). ---

fn outbox_item(id: i64, not_before: chrono::DateTime<chrono::Utc>, failed: bool) -> lookout_mail::session::OutboxItem {
    lookout_mail::session::OutboxItem {
        id,
        not_before,
        failed,
        attempts: 0,
        subject: format!("message {id}"),
        recipients: vec![],
        draft_message_id: None,
    }
}

#[test]
fn outbox_banner_names_failed_then_scheduled_then_waiting() {
    let now = chrono::Utc::now();
    let later = now + chrono::Duration::hours(1);
    let earlier = now - chrono::Duration::minutes(5);
    assert_eq!(outbox_banner_text(&[], now), None);
    let scheduled = [outbox_item(1, later, false), outbox_item(2, later, false)];
    assert_eq!(outbox_banner_text(&scheduled.iter().collect::<Vec<_>>(), now).as_deref(), Some("2 scheduled messages"));
    let waiting = [outbox_item(1, later, false), outbox_item(2, earlier, false)];
    assert_eq!(outbox_banner_text(&waiting.iter().collect::<Vec<_>>(), now).as_deref(), Some("2 messages waiting to send"));
    let one_failed = [outbox_item(1, later, false), outbox_item(2, earlier, true)];
    assert_eq!(
        outbox_banner_text(&one_failed.iter().collect::<Vec<_>>(), now).as_deref(),
        Some("1 message couldn't be sent")
    );
    assert_eq!(outbox_banner_text(&[&outbox_item(3, later, false)], now).as_deref(), Some("1 scheduled message"));
}

// --- Pane-width restore maths. Pure, so unlike the widget suites further
// down these need no display and no `gtk_test::gtk_ready()` gate. ---

#[test]
fn a_stored_percentage_restores_proportionally_at_a_wider_window() {
    // The regression guard for the reported bug: a list dragged to 400px
    // in a 1600px window comes back 640px wide when the window is
    // maximized to 2560px, not at whatever default it happened to hold.
    let pct = pane_width_percent(400, 1600).expect("a real window has a width");
    assert_eq!(pct, 25.0);
    assert_eq!(
        paned_position_for_percent(pct, 1600, 1500, 100, 300, None),
        Some(400),
        "same width: the drag round-trips exactly"
    );
    assert_eq!(
        paned_position_for_percent(pct, 2560, 2400, 100, 300, None),
        Some(640),
        "maximized: the same share of a wider window"
    );
}

#[test]
fn an_unallocated_pane_declines_so_the_restore_is_retried() {
    // A pane maps before it is allocated; restoring against a width of 0
    // would collapse it to its minimum, so the caller is told to come back.
    assert_eq!(paned_position_for_percent(25.0, 1600, 0, 100, 300, None), None);
    assert_eq!(paned_position_for_end_percent(25.0, 1600, 0, 100, 300), None);
    assert_eq!(paned_position_for_percent(25.0, 0, 1500, 100, 300, None), None, "no window width to be a percentage of");
}

#[test]
fn a_pane_that_was_never_dragged_has_nothing_to_restore() {
    // -1 is the GSettings default for every `pane-*-width-percent` key.
    assert_eq!(paned_position_for_percent(-1.0, 1600, 1500, 100, 300, None), None);
    assert_eq!(paned_position_for_end_percent(-1.0, 1600, 1500, 100, 300), None);
}

#[test]
fn a_restored_width_stays_within_the_panes_own_bounds() {
    assert_eq!(
        paned_position_for_percent(1.0, 1600, 1500, 240, 300, None),
        Some(240),
        "clamped up to the start child's minimum"
    );
    assert_eq!(
        paned_position_for_percent(90.0, 1600, 1500, 100, 300, None),
        Some(1200),
        "clamped down to leave the end child its minimum"
    );
    assert_eq!(
        paned_position_for_percent(50.0, 1600, 1500, 100, 300, Some(FOLDER_PANE_MAX_WIDTH)),
        Some(FOLDER_PANE_MAX_WIDTH),
        "the folder pane's ceiling applies to a restore as well as to a drag"
    );
}

#[test]
fn a_window_too_narrow_for_both_children_does_not_panic() {
    // `i32::clamp` panics when min > max, which is exactly what a paned
    // narrower than its two children's minimums produces.
    assert_eq!(paned_position_for_percent(25.0, 1600, 300, 400, 500, None), Some(400));
    assert_eq!(paned_position_for_percent(25.0, 1600, 300, 400, 500, Some(FOLDER_PANE_MAX_WIDTH)), Some(400));
    assert_eq!(paned_position_for_end_percent(25.0, 1600, 300, 400, 500), Some(0));
}

#[test]
fn the_overview_percentage_is_of_the_end_childs_own_width() {
    // 20% of a 2000px window is a 400px overview pane, so the separator
    // sits 400px from the paned's right edge.
    assert_eq!(paned_position_for_end_percent(20.0, 2000, 1900, 300, 200), Some(1500));
    assert_eq!(paned_position_for_end_percent(1.0, 2000, 1900, 300, 200), Some(1700), "clamped to the pane's own minimum");
}

#[test]
fn nothing_is_stored_for_a_window_with_no_width() {
    assert_eq!(pane_width_percent(400, 0), None);
    assert_eq!(pane_width_percent(400, -1), None);
}

#[test]
fn a_restored_width_does_not_creep_a_pixel_on_every_launch() {
    // Taken from a real session: a 232px overview pane in a 2880px window
    // stores 8.055555555555555%, and replaying that against 2880 gives
    // 231.99999999999997. Truncating lost the pixel, and the narrower pane
    // was then saved as a smaller percentage - so the pane shrank by one
    // pixel every launch. Both directions of the round trip must be exact.
    let pct = pane_width_percent(232, 2880).expect("a real window has a width");
    assert_eq!(paned_position_for_end_percent(pct, 2880, 2818, 1174, 226), Some(2818 - 232));
    let pct = pane_width_percent(934, 2880).expect("a real window has a width");
    assert_eq!(paned_position_for_percent(pct, 2880, 2260, 387, 563, None), Some(934));
}

#[test]
fn only_a_drag_within_an_unchanged_allocation_is_saved() {
    let restored = Some(PaneRestoreState {
        paned_extent: 1500,
        position: 400,
    });
    assert!(
        pane_change_is_a_drag(restored, 1500, 520),
        "the separator moved while the paned stayed the same size: a drag"
    );
    assert!(!pane_change_is_a_drag(restored, 1500, 400), "the position didn't actually change");
}

#[test]
fn a_gtk_reclamp_after_an_allocation_change_is_never_saved() {
    // The regression this whole change exists for. GtkPaned re-clamps its
    // position from a layout pass whenever its allocation changes, and
    // emits `notify::position` from there - asynchronously, after the
    // restore's `pane_widths_applying` flag has already been cleared. The
    // changed `paned_width` is what gives it away.
    let restored = Some(PaneRestoreState {
        paned_extent: 1500,
        position: 400,
    });
    assert!(!pane_change_is_a_drag(restored, 2400, 400), "window widened: the paned grew, so this is a re-fit");
    assert!(!pane_change_is_a_drag(restored, 900, 300), "window narrowed: the position was clamped down to fit");
}

#[test]
fn a_pane_that_has_not_been_restored_yet_never_saves() {
    // No record means the pane is still showing its built-in default, or
    // GTK's allocation-time clamp of it - neither is a width the user
    // chose, and saving one would overwrite the remembered percentage.
    assert!(!pane_change_is_a_drag(None, 1500, 400));
    assert!(!pane_change_is_a_drag(None, 0, 0));
}

#[test]
fn a_window_width_is_only_trusted_once_two_attempts_agree() {
    assert!(!window_width_is_stable(None, 1600), "the first reading could be the pre-maximize width");
    assert!(!window_width_is_stable(Some(1600), 2880), "still moving: the maximize just landed");
    assert!(window_width_is_stable(Some(2880), 2880), "two attempts agree, so this is the settled width");
}

#[test]
fn a_window_with_no_width_is_never_stable() {
    // An unallocated window reports 0, and two zeroes in a row still say
    // nothing about the width the panes will end up being laid out in.
    assert!(!window_width_is_stable(Some(0), 0));
    assert!(!window_width_is_stable(Some(-1), -1));
}

fn test_mailbox(account: &AccountId, name: &str, unread: u32) -> Mailbox {
    Mailbox {
        id: MailboxId::new(account, name),
        account_id: account.clone(),
        name: name.to_string(),
        parent: None,
        delimiter: '/',
        role: MailboxRole::Inbox,
        uidvalidity: UidValidity(1),
        uidnext: 1,
        highest_modseq: None,
        total: 0,
        unread,
        flags: vec![],
        subscribed: true,
    }
}

#[test]
fn folder_tree_signature_tracks_the_unread_count() {
    let account = AccountId("acc".into());
    let before = folder_tree_signature(&[(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 3)])], &[], &HashMap::new());
    let after = folder_tree_signature(&[(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 4)])], &[], &HashMap::new());
    assert_ne!(before, after, "a changed count must rebuild the tree - it's what the row draws");
}

#[test]
fn goa_accounts_are_enabled_by_default_and_toggle_persists() {
    let state = test_state(Vec::new());
    let id = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".into());
    let other = AccountId("/org/gnome/OnlineAccounts/Accounts/account_2".into());
    assert!(state.borrow().account_enabled(&id), "accounts are enabled by default");
    state.borrow().set_account_enabled(&id, false);
    assert!(!state.borrow().account_enabled(&id));
    // Disabling one account leaves the others enabled.
    assert!(state.borrow().account_enabled(&other));
    // Re-enabling works.
    state.borrow().set_account_enabled(&id, true);
    assert!(state.borrow().account_enabled(&id));
}

fn attachment(part_number: &str, filename: Option<&str>) -> BodyPart {
    BodyPart {
        part_number: part_number.to_string(),
        content_type: "application/pdf".to_string(),
        charset: None,
        transfer_encoding: Some("base64".to_string()),
        filename: filename.map(str::to_string),
        cid: None,
        size: 0,
        is_attachment: true,
    }
}

#[test]
fn attachment_display_name_prefers_filename_and_sanitizes_placeholders() {
    assert_eq!(attachment_display_name(&attachment("2", Some("report.pdf"))), "report.pdf");
    // A filename that could smuggle a path separator must not reach the
    // save dialog as a path - fall back to the part-number placeholder.
    assert_eq!(attachment_display_name(&attachment("2", Some("../etc/passwd"))), "attachment-2");
    assert_eq!(attachment_display_name(&attachment("2", Some(""))), "attachment-2");
    assert_eq!(attachment_display_name(&attachment("2", None)), "attachment-2");
    assert_eq!(attachment_display_name(&attachment("1.3", None)), "attachment-1.3");
}

#[test]
fn duplicate_attachment_names_are_made_unique() {
    let mut used = std::collections::HashSet::new();
    assert_eq!(unique_attachment_name(&attachment("1", Some("report.pdf")), &mut used), "report.pdf");
    assert_eq!(unique_attachment_name(&attachment("2", Some("report.pdf")), &mut used), "report (2).pdf");
    assert_eq!(unique_attachment_name(&attachment("3", Some("report.pdf")), &mut used), "report (3).pdf");
    assert_eq!(unique_attachment_name(&attachment("4", Some("notes")), &mut used), "notes");
    assert_eq!(unique_attachment_name(&attachment("5", Some("notes")), &mut used), "notes (2)");
}

/// The .eml export dialog's suggested name comes from the message's
/// subject, with a path-free placeholder for empty or hostile subjects -
/// the raw subject is untrusted header data and must not reach the dialog
/// as a path.
#[test]
fn eml_suggested_name_uses_subject_and_falls_back_safely() {
    let with_subject = |subject: Option<&str>| EmailSummary {
        subject: subject.map(str::to_string),
        ..summary(Uid(1), "INBOX", 2026, 7, 30, 12)
    };
    assert_eq!(eml_suggested_name(&with_subject(Some("Quarterly report"))), "Quarterly report.eml");
    // Whitespace-only and missing subjects fall back to the placeholder.
    assert_eq!(eml_suggested_name(&with_subject(Some("  "))), "message.eml");
    assert_eq!(eml_suggested_name(&with_subject(None)), "message.eml");
    // A subject that could smuggle a path separator must not reach the
    // save dialog as a path.
    assert_eq!(eml_suggested_name(&with_subject(Some("../etc/passwd"))), "message.eml");
    assert_eq!(eml_suggested_name(&with_subject(Some("a\\b"))), "message.eml");
}

#[test]
fn part_content_type_defaults_only_when_the_wire_gave_none() {
    let part = |content_type: &str| BodyPart {
        part_number: "2".to_string(),
        content_type: content_type.to_string(),
        charset: None,
        transfer_encoding: None,
        filename: None,
        cid: None,
        size: 0,
        is_attachment: true,
    };
    assert_eq!(part_content_type(&part("application/pdf")), "application/pdf");
    assert_eq!(part_content_type(&part("")), "application/octet-stream");
    assert_eq!(part_content_type(&part("  ")), "application/octet-stream");
}

/// The details dialog's key fields come from the summary: From/To always,
/// Cc only when present, and Message ID only when the envelope has one.
#[test]
fn message_detail_fields_include_cc_and_message_id_only_when_present() {
    let sender = EmailAddress {
        name: Some("Ada Lovelace".to_string()),
        address: "ada@example.com".to_string(),
    };
    let base = EmailSummary {
        from: vec![sender.clone()],
        ..summary(Uid(1), "INBOX", 2026, 7, 30, 12)
    };
    let fields = message_detail_fields(&base);
    let captions: Vec<&str> = fields.iter().map(|(caption, _)| caption.as_str()).collect();
    assert_eq!(captions, vec!["From", "To", "Subject", "Date"]);
    assert!(fields.iter().any(|(_, value)| value == "Ada Lovelace"), "From should render the sender");

    let full = EmailSummary {
        cc: base.from.clone(),
        message_id: Some("<id@example.com>".to_string()),
        ..base
    };
    let captions: Vec<String> = message_detail_fields(&full).into_iter().map(|(caption, _)| caption).collect();
    assert_eq!(captions, vec!["From", "To", "Cc", "Subject", "Date", "Message ID"]);
}

/// The header block renders one `Name: value` line per parsed header.
#[test]
fn format_header_block_renders_name_value_lines() {
    let headers = vec![("From".to_string(), "Ada <ada@example.com>".to_string()), ("Subject".to_string(), "Hi".to_string())];
    assert_eq!(format_header_block(&headers), "From: Ada <ada@example.com>\nSubject: Hi");
}

/// The source view's text passes small messages through lossily, and
/// truncates large ones with an announced byte count.
#[test]
fn message_source_text_truncates_past_the_display_cap() {
    assert_eq!(message_source_text(b"Subject: Hi\r\n\r\nBody"), "Subject: Hi\r\n\r\nBody");

    let mut large = vec![b'a'; MESSAGE_SOURCE_DISPLAY_CAP];
    large.extend_from_slice(b"\nmore bytes past the cap");
    let text = message_source_text(&large);
    assert!(text.starts_with(&"a".repeat(16)));
    assert!(text.ends_with(&format!(
        "… truncated: showing the first {MESSAGE_SOURCE_DISPLAY_CAP} of {} bytes. Use \"Save as .eml…\" for the full message.",
        large.len()
    )));
}

fn attachment_with_type(part_number: &str, filename: Option<&str>, content_type: &str) -> BodyPart {
    BodyPart {
        part_number: part_number.to_string(),
        content_type: content_type.to_string(),
        charset: None,
        transfer_encoding: Some("base64".to_string()),
        filename: filename.map(str::to_string),
        cid: None,
        size: 0,
        is_attachment: true,
    }
}

/// The temp-file copy's extension decides which handler opens it, so the
/// filename's own extension wins when clean, and content-type mappings
/// cover the no-name and hostile-name cases.
#[test]
fn attachment_extension_prefers_clean_filename_extensions() {
    assert_eq!(attachment_extension(&attachment_with_type("2", Some("report.pdf"), "application/pdf")), "pdf");
    // Lowercased - handlers expect conventional extensions.
    assert_eq!(attachment_extension(&attachment_with_type("2", Some("Photo.PNG"), "image/png")), "png");
    // A hostile or degenerate filename's extension is ignored, and the
    // content type answers instead.
    assert_eq!(attachment_extension(&attachment_with_type("2", Some("report.<script>"), "application/pdf")), "pdf");
    assert_eq!(attachment_extension(&attachment_with_type("2", Some("report."), "application/pdf")), "pdf");
    assert_eq!(attachment_extension(&attachment_with_type("2", Some("noext"), "application/pdf")), "pdf");
}

#[test]
fn attachment_extension_falls_back_through_content_type_to_bin() {
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/pdf")), "pdf");
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "image/jpeg")), "jpg");
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "message/rfc822")), "eml");
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "text/csv")), "csv");
    assert_eq!(
        attachment_extension(&attachment_with_type("2", None, "application/vnd.openxmlformats-officedocument.wordprocessingml.document")),
        "docx"
    );
    // Unknown types: a single clean subtype token is kept, anything with
    // punctuation or over-length falls back to `bin`.
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/xfoobar")), "xfoobar");
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/octet-stream")), "bin");
    assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/vnd.example-thing")), "bin");
}

/// The temp-file name must be unique per call, carry the attachment's
/// extension (so the handler recognizes it), and never smuggle path
/// separators into the file name.
#[test]
fn temp_attachment_path_is_unique_named_and_sanitized() {
    let part = attachment_with_type("2", Some("report.pdf"), "application/pdf");
    let a = temp_attachment_path(&part);
    let b = temp_attachment_path(&part);
    assert_ne!(a, b, "each open materializes its own copy");
    assert_eq!(a.parent(), std::env::temp_dir().as_path().into());
    let name = a.file_name().and_then(|n| n.to_str()).expect("utf-8 temp name");
    assert!(name.starts_with("lookout-"), "identifiable as Lookout's: {name}");
    assert!(name.ends_with(".pdf"), "the handler's extension must survive: {name}");
    assert_eq!(name.matches('.').count(), 1, "uuid + ext, nothing else: {name}");
    assert!(!name.contains('/') && !name.contains('\\'));
}

#[test]
fn folder_tree_signature_ignores_fields_the_tree_never_draws() {
    // A STATUS pass rewrites uidnext/uidvalidity on every folder every
    // time. If those counted, the guard would fire on every pass and the
    // sidebar would rebuild (collapsing subfolders, dropping the
    // highlight) for changes that are invisible.
    let account = AccountId("acc".into());
    let mut noisy = test_mailbox(&account, "INBOX", 3);
    noisy.uidnext = 999;
    noisy.uidvalidity = UidValidity(42);
    noisy.highest_modseq = Some(7);
    let quiet = folder_tree_signature(&[(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 3)])], &[], &HashMap::new());
    let churned = folder_tree_signature(&[(account.clone(), "Me".into(), vec![noisy])], &[], &HashMap::new());
    assert_eq!(quiet, churned);
}

#[test]
fn folder_tree_signature_tracks_totals_and_display_prefs() {
    // The total is drawn too now - by a row set to show it, and in every
    // row's "⋯" menu heading - and a colour or count-mode change has to
    // repaint the row it belongs to.
    let account = AccountId("acc".into());
    let inbox = test_mailbox(&account, "INBOX", 3);
    let accounts = vec![(account.clone(), "Me".to_string(), vec![inbox.clone()])];
    let base = folder_tree_signature(&accounts, &[], &HashMap::new());
    let mut grown = inbox.clone();
    grown.total += 1;
    assert_ne!(base, folder_tree_signature(&[(account.clone(), "Me".into(), vec![grown])], &[], &HashMap::new()));
    let colored = HashMap::from([(
        inbox.id.clone(),
        crate::ui_state_db::FolderPrefs {
            color: Some("#3584e4".into()),
            ..Default::default()
        },
    )]);
    assert_ne!(base, folder_tree_signature(&accounts, &[], &colored));
}

#[test]
fn folder_tree_signature_tracks_the_favorites_section() {
    let account = AccountId("acc".into());
    let accounts = [(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 0)])];
    let without = folder_tree_signature(&accounts, &[], &HashMap::new());
    let with = folder_tree_signature(&accounts, &[test_mailbox(&account, "INBOX", 0)], &HashMap::new());
    assert_ne!(without, with, "starring a folder adds a whole section and must rebuild");
}

#[test]
fn body_request_matches_the_current_pending_selection() {
    let pending = Some((MailboxId("acc:inbox".into()), Uid(42)));
    assert!(body_request_matches(&MailboxId("acc:inbox".into()), &Uid(42), pending.as_ref()));
    assert!(!body_request_matches(&MailboxId("acc:inbox".into()), &Uid(43), pending.as_ref()));
}

#[test]
fn body_request_is_rejected_when_no_selection_is_pending() {
    assert!(!body_request_matches(&MailboxId("acc:inbox".into()), &Uid(42), None));
}

/// The unavailable page blames the right thing: an offline device gets
/// told to check its connection, while a reachable network points at the
/// server instead.
#[test]
fn body_fetch_error_description_names_offline_vs_server() {
    let offline = body_fetch_error_description(false);
    assert!(offline.contains("offline") && offline.contains("network connection"));
    let server = body_fetch_error_description(true);
    assert!(server.contains("server") && !server.contains("offline"));
}

#[test]
fn mailbox_account_id_splits_the_account_prefix() {
    assert_eq!(
        mailbox_account_id(&MailboxId("/org/gnome/OnlineAccounts/Accounts/account_7:INBOX".into())),
        Some(AccountId("/org/gnome/OnlineAccounts/Accounts/account_7".into()))
    );
    // No account prefix (an id not shaped "account:path").
    assert_eq!(mailbox_account_id(&MailboxId("INBOX".into())), None);
}

#[test]
fn search_copies_collapse_to_the_most_specific_folder_per_account() {
    let hit = |uid: u32, mailbox: &str, message_id: Option<&str>| EmailSummary {
        message_id: message_id.map(str::to_string),
        ..summary(Uid(uid), mailbox, 2026, 10, 9, 9)
    };
    let results = vec![
        hit(1, "gmail:[Gmail]/All Mail", Some("<a@x>")),
        hit(2, "gmail:Work", Some("<a@x>")),
        hit(3, "gmail:INBOX", Some("a@x")),
        hit(4, "gmail:[Gmail]/All Mail", Some("<b@x>")),
        hit(5, "gmail:[Gmail]/Bin", Some("<b@x>")),
        hit(6, "other:INBOX", Some("<a@x>")),
        hit(7, "gmail:Work", None),
        hit(8, "gmail:INBOX", None),
    ];
    let role_of = |mailbox: &MailboxId| {
        Some(match mailbox.0.split_once(':').map(|(_, path)| path) {
            Some("INBOX") => MailboxRole::Inbox,
            Some("[Gmail]/All Mail") => MailboxRole::Archive,
            Some("[Gmail]/Bin") => MailboxRole::Trash,
            _ => MailboxRole::Custom,
        })
    };
    let kept: Vec<u32> = collapse_search_copies(&results, role_of).iter().map(|m| m.uid.0).collect();
    // `<a@x>` keeps its Inbox copy (angle brackets don't matter), `<b@x>`
    // prefers All Mail over the Bin, another account's copy is its own
    // message, and Message-ID-less hits are never merged.
    assert_eq!(kept, vec![3, 4, 6, 7, 8]);
}

#[test]
fn csp_blocks_remote_content_unless_the_level_allows_it() {
    // Nothing allowed: no `*` source anywhere, so remote images, styles,
    // fonts and media are all vetoed; the message's own data:/cid: images
    // and inline styles still pass.
    let blocked = message_content_security_policy(false, false);
    assert!(blocked.contains("img-src data: cid:") && !blocked.contains("img-src *"));
    assert!(blocked.contains("script-src 'none'") && blocked.contains("frame-src 'none'") && blocked.contains("object-src 'none'"));
    assert!(blocked.contains("base-uri 'none'") && blocked.contains("form-action 'none'"));
    assert!(blocked.contains("style-src 'unsafe-inline'"));
    // Images only: remote image loads pass, everything else remote stays blocked.
    let images = message_content_security_policy(true, false);
    assert!(images.contains("img-src * data: cid:"));
    assert!(!images.contains("font-src *") && !images.contains("media-src *"));
    // All content: remote styles/fonts/media pass too.
    let all = message_content_security_policy(true, true);
    assert!(all.contains("default-src *"));
    // JavaScript and frames are vetoed at every level - a trusted sender
    // still never gains navigation or scripting.
    for csp in [blocked, images, all] {
        assert!(csp.contains("script-src 'none'"));
        assert!(csp.contains("frame-src 'none'"));
    }
}

#[test]
fn wrap_message_with_csp_prepends_to_fragments_and_slips_past_the_doctype() {
    // Fragment HTML (no head): the meta goes first, ahead of every
    // content element the CSP spec counts.
    let wrapped = wrap_message_with_csp("<p>hello</p>", false, false);
    assert!(wrapped.starts_with("<meta http-equiv=\"Content-Security-Policy\""));
    assert!(wrapped.ends_with("<p>hello</p>"));
    // Leading whitespace is insignificant in HTML; the meta stays the
    // first element and the body's bytes are preserved in order.
    let wrapped = wrap_message_with_csp("\n\n<p>hello</p>", false, false);
    assert!(wrapped.starts_with("<meta"));
    assert!(wrapped.ends_with("\n\n<p>hello</p>"));
    // A doctyped document keeps the doctype first (so it stays in
    // standards mode) and the meta lands immediately after it.
    let wrapped = wrap_message_with_csp("<!DOCTYPE html><html><body><p>x</p></body></html>", false, false);
    assert!(wrapped.starts_with("<!DOCTYPE html><meta"));
    assert!(wrapped.ends_with("</html>"));
    assert_eq!(wrapped.matches("http-equiv=\"Content-Security-Policy\"").count(), 1);
    // Case-insensitive doctype.
    let wrapped = wrap_message_with_csp("<!doctype html><html></html>", true, true);
    assert!(wrapped.starts_with("<!doctype html><meta"));
}

/// A minimal vCard with one email field, for contact-lookup tests.
fn test_contact(address: &str) -> VCard {
    VCard {
        version: "4.0".to_string(),
        kind: None,
        uid: Some("uid".into()),
        full_name: Some("Ada Lovelace".into()),
        name: None,
        organization: None,
        title: None,
        file_as: None,
        photo: None,
        emails: vec![lookout_core::EmailField {
            types: vec!["work".into()],
            address: address.to_string(),
        }],
        telephones: Vec::new(),
        addresses: Vec::new(),
        urls: Vec::new(),
        impp: Vec::new(),
        note: None,
        birthday: None,
        anniversary: None,
        categories: Vec::new(),
        other: Vec::new(),
    }
}

#[test]
fn find_contact_by_address_matches_across_accounts_case_insensitively() {
    let state = test_state(Vec::new());
    {
        let mut st = state.borrow_mut();
        // The matching card lives in the alphabetically later account,
        // so a match proves the search spans accounts rather than
        // picking a lucky first.
        st.contacts_by_account.insert(
            AccountId("account_b".into()),
            crate::contacts_view::test_snapshot(
                "Beta",
                vec![lookout_dav::ContactRecord {
                    href: "/b/ada.vcf".into(),
                    etag: Some("etag-b".into()),
                    card: test_contact("Ada.Lovelace@Example.com"),
                }],
            ),
        );
        st.contacts_by_account.insert(
            AccountId("account_a".into()),
            crate::contacts_view::test_snapshot(
                "Alpha",
                vec![lookout_dav::ContactRecord {
                    href: "/a/grace.vcf".into(),
                    etag: None,
                    card: test_contact("grace@example.com"),
                }],
            ),
        );
    }
    // Case/whitespace-insensitive exact match on the address string.
    let (account_id, entry) = find_contact_by_address(&state, "  ada.lovelace@example.com ", None).expect("found in an address book");
    assert_eq!(account_id, AccountId("account_b".into()));
    assert_eq!(entry.card.emails[0].address, "Ada.Lovelace@Example.com");
    assert_eq!(entry.href, "/b/ada.vcf");
    assert_eq!(entry.etag.as_deref(), Some("etag-b"));
    // The email's own account is preferred when it also has the contact.
    state
        .borrow_mut()
        .contacts_by_account
        .get_mut(&AccountId("account_a".into()))
        .unwrap()
        .contacts
        .push(lookout_dav::ContactRecord {
            href: "/a/ada.vcf".into(),
            etag: None,
            card: test_contact("ada.lovelace@example.com"),
        });
    let (preferred_id, _) = find_contact_by_address(&state, "ada.lovelace@example.com", Some(&AccountId("account_a".into()))).expect("found");
    assert_eq!(preferred_id, AccountId("account_a".into()));
    // No address book has the address - nothing found.
    assert!(find_contact_by_address(&state, "nobody@example.com", None).is_none());
}

#[test]
fn unified_merge_dedupes_by_mailbox_and_uid_and_sorts_newest_first() {
    let snapshots = HashMap::from([
        (
            MailboxId("a:INBOX".into()),
            vec![summary(Uid(1), "a:INBOX", 2024, 1, 10, 9), summary(Uid(2), "a:INBOX", 2024, 1, 10, 8)],
        ),
        (
            MailboxId("b:INBOX".into()),
            // Duplicate of a:INBOX/2 (same uid, different mailbox) is kept;
            // the doubled a:INBOX/1 in this snapshot must collapse.
            vec![summary(Uid(2), "b:INBOX", 2024, 1, 10, 10), summary(Uid(1), "a:INBOX", 2024, 1, 10, 9)],
        ),
    ]);

    let merged = merge_unified_snapshots(&snapshots);
    let keys: Vec<(String, u32)> = merged.iter().map(|m| (m.mailbox.0.clone(), m.uid.0)).collect();
    // Newest first; a:INBOX/1 appears once despite being in both snapshots.
    assert_eq!(keys, vec![("b:INBOX".into(), 2), ("a:INBOX".into(), 1), ("a:INBOX".into(), 2)]);
}

#[test]
fn unified_merge_breaks_a_date_tie_by_mailbox_then_uid() {
    // Two messages sharing an identical date, in different mailboxes.
    // `HashMap::values()` iteration order is unspecified, so without an
    // explicit tie-break this assertion would be flaky by construction -
    // it must hold regardless of which snapshot the map happens to
    // iterate first.
    let snapshots = HashMap::from([
        (MailboxId("z:INBOX".into()), vec![summary(Uid(1), "z:INBOX", 2024, 1, 10, 9)]),
        (MailboxId("a:INBOX".into()), vec![summary(Uid(5), "a:INBOX", 2024, 1, 10, 9)]),
    ]);
    let keys: Vec<(String, u32)> = merge_unified_snapshots(&snapshots).iter().map(|m| (m.mailbox.0.clone(), m.uid.0)).collect();
    assert_eq!(
        keys,
        vec![("a:INBOX".into(), 5), ("z:INBOX".into(), 1)],
        "equal dates must break on mailbox, not map iteration order"
    );
}

/// A minimal `UiState` with one `AccountHandle` per given account (fresh
/// command channels, empty folder lists unless the caller passes them).
fn test_state(accounts: Vec<(AccountId, Vec<Mailbox>)>) -> Rc<RefCell<UiState>> {
    let accounts: HashMap<AccountId, AccountHandle> = accounts
        .into_iter()
        .map(|(id, folders)| {
            let (cmd_tx, _cmd_rx) = async_channel::unbounded();
            let (interactive_cmd_tx, _interactive_cmd_rx) = async_channel::unbounded();
            (
                id.clone(),
                AccountHandle {
                    cmd_tx,
                    interactive_cmd_tx,
                    email: "a@b.c".into(),
                    display_name: String::new(),
                    imap_host: "imap".into(),
                    imap_port: 993,
                    smtp_host: "smtp".into(),
                    smtp_port: 465,
                    folders,
                    address_cache: None,
                    graph_pin: None,
                    graph_mail: None,
                    graph_categories: None,
                    graph_rules: None,
                    graph_automatic_replies: None,
                    sieve_advertised: false,
                },
            )
        })
        .collect();
    Rc::new(RefCell::new(UiState {
        accounts,
        contacts_by_account: HashMap::new(),
        starred_contacts: HashSet::new(),
        contacts_filter: String::new(),
        contacts_refresh: None,
        ui_db: None,
        settings: Rc::new(crate::settings::resolve()),
        app_config: Rc::new(RefCell::new(crate::app_config::AppConfig::default())),
        categories: Rc::new(RefCell::new(crate::categories::CategorySet::default())),
        deleted_contacts: HashMap::new(),
        contact_cmd_tx: HashMap::new(),
        goa_accounts: HashMap::new(),
        goa_client: None,
        goa_unavailable: false,
        app: None,
        current_account: None,
        current_mailbox: None,
        pending_message_selection: None,
        mail_view: MailView::Single,
        unified_snapshots: HashMap::new(),
        mailbox_sync_epoch: HashMap::new(),
        pending_optimistic_removals: HashMap::new(),
        pending_optimistic_flag_changes: HashMap::new(),
        pending_optimistic_pinned_changes: HashMap::new(),
        pending_body_request: None,
        body_fetch_error: None,
        body_fetch_generation: 0,
        pending_attachment: None,
        pending_attachment_batch: None,
        pending_raw_message: None,
        pending_report_phishing: None,
        pending_rule_forwards: Vec::new(),
        pending_rule_evals: Vec::new(),
        message_windows: HashMap::new(),
        message_details: None,
        rendered_draft: None,
        rendered_draft_message_id: None,
        rendered_snooze: None,
        rendered_follow_up: None,
        flagged_mail: HashMap::new(),
        outbox: HashMap::new(),
        outbox_banner: None,
        outbox_dialog: None,
        sieve_support: HashMap::new(),
        automatic_replies: HashMap::new(),
        automatic_replies_banner: None,
        automatic_replies_dialog_refresh: None,
        pending_draft_open: None,
        pending_forward_raw: None,
        unsubscribe_info: None,
        unsubscribe_dismissed: None,
        imip: None,
        imip_dismissed: None,
        imip_replies_applied: HashSet::new(),
        calendar_state: None,
        read_receipt_request: None,
        read_receipt_dismissed: None,
        read_receipts_sent: HashSet::new(),
        read_receipt_context: None,
        pending_cid: HashMap::new(),
        rendered_conversation: None,
        conversation_inline_parts: HashMap::new(),
        conversation_ui: None,
        conversation_mark_on_leave: Vec::new(),
        rendered_inline_parts: Vec::new(),
        temp_attachment_files: HashSet::new(),
        toast_overlay: None,
        pending_sends: Vec::new(),
        next_send_token: 1,
        undo_stack: Vec::new(),
        next_undo_id: 0,
        pending_html_reveal: false,
        pending_header: None,
        body_cache: BodyCache::new(BODY_CACHE_IN_MEMORY),
        reveal_generation: 0,
        last_selection: None,
        restore_pending: false,
        rendered_message: None,
        pending_mark_read: None,
        mark_read_on_leave: None,
        syncing: HashSet::new(),
        prefetching: HashSet::new(),
        folder_row_spinners: HashMap::new(),
        refreshing: HashSet::new(),
        sort_key: SortKey::Date,
        sort_descending: true,
        favorites: Vec::new(),
        favorite_display_order: Vec::new(),
        folder_prefs: HashMap::new(),
        recent_folders: HashMap::new(),
        recent_searches: Vec::new(),
        search_scope: SearchScope::All,
        body_waiter: None,
        load_remote_images: false,
        rich_text_default: true,
        trusted_senders: HashMap::new(),
        blocked_senders: HashSet::new(),
        rendered_trust_sender: None,
        rendered_from_mismatch: None,
        rendered_link_warnings: Vec::new(),
        load_once_images: false,
        message_theme_override: false,
        trust_banner_dismissed: None,
        rendered_remote_scan: None,
        draft_saved_tx: None,
        composer_identities_refresh: None,
        manage_signatures: None,
        manage_templates: None,
        create_task_from_message: None,
        reply_with_meeting: None,
        flagged_mail_refresh: None,
        composer_relay_generation: 0,
        compose_popout_window: None,
        compose_action_bar: None,
        folder_tree: None,
        expanded_folders: HashSet::new(),
        collapsed_groups: HashSet::new(),
        suppress_folder_selection: false,
        search_active: false,
        search_query: String::new(),
        search_results: Vec::new(),
        search_pending: HashSet::new(),
        search_entry: None,
        search_ribbon: None,
        search_ribbon_prev: None,
    }))
}

#[test]
fn request_mailbox_sync_dedupes_until_answered_or_reconnected() {
    let account_id = AccountId("acc".into());
    let inbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id.clone(), Vec::new())]);

    // First request goes out and is marked pending.
    assert!(request_mailbox_sync(&state, &account_id, &inbox));
    // A duplicate while the first is still in flight is dropped.
    assert!(!request_mailbox_sync(&state, &account_id, &inbox));

    // The sync landing clears the pending mark, so a later request - a
    // genuine re-sync - is allowed again.
    state.borrow_mut().syncing.remove(&inbox);
    assert!(request_mailbox_sync(&state, &account_id, &inbox));

    // A reconnect (fresh folders) also clears it, so a request that died
    // with a dropped connection can't suppress the next one.
    state.borrow_mut().syncing.insert(inbox.clone());
    state.borrow_mut().syncing.retain(|mailbox| mailbox_account_id(mailbox).as_ref() != Some(&account_id));
    assert!(!state.borrow().syncing.contains(&inbox));
}

/// `optimistic_remove_messages` hides a row (and its unified-view
/// snapshot copy) the instant it's called, stashing it away; a matching
/// `restore_optimistic_removals` (the `MoveFailed` rollback path) puts it
/// straight back, in both places, and clears the stash. Same self-skip
/// convention as the other GTK-touching tests in this module.
#[test]
fn optimistic_remove_and_restore_round_trips_the_list_and_unified_snapshot() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    let three = summary(Uid(3), "acc:INBOX", 2026, 8, 1, 11);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone(), three.clone()], SortKey::Date, true);
    state.borrow_mut().unified_snapshots.insert(mailbox.clone(), vec![one.clone(), two.clone(), three.clone()]);

    optimistic_remove_messages(&state, &message_list, &mailbox, &[Uid(2)]);
    let uids_after_remove: HashSet<u32> = message_list.all_messages().iter().map(|m| m.uid.0).collect();
    assert_eq!(uids_after_remove, HashSet::from([1, 3]), "the row must vanish immediately, before any server round trip");
    assert_eq!(
        state.borrow().unified_snapshots[&mailbox].iter().map(|m| m.uid.0).collect::<HashSet<_>>(),
        HashSet::from([1, 3]),
        "the unified-view snapshot must lose the row too, or a later merge would resurrect it"
    );
    assert_eq!(
        state.borrow().pending_optimistic_removals[&mailbox].iter().map(|m| m.uid.0).collect::<Vec<_>>(),
        vec![2],
        "the hidden row must be stashed for a possible rollback"
    );

    restore_optimistic_removals(&state, &message_list, &mailbox, &[Uid(2)]);
    let uids_after_restore: HashSet<u32> = message_list.all_messages().iter().map(|m| m.uid.0).collect();
    assert_eq!(uids_after_restore, HashSet::from([1, 2, 3]), "a failed move must restore exactly the row it hid");
    assert_eq!(
        state.borrow().unified_snapshots[&mailbox].iter().map(|m| m.uid.0).collect::<HashSet<_>>(),
        HashSet::from([1, 2, 3]),
        "the unified-view snapshot must get the row back too"
    );
    assert!(
        !state.borrow().pending_optimistic_removals.contains_key(&mailbox),
        "the stash must be empty once its only entry is restored"
    );
}

/// `optimistic_toggle_read` flips `SystemFlagBit::Seen` (and the
/// unified-view snapshot's copy) the instant it's called, stashing the
/// pre-toggle summary; a matching `restore_optimistic_flag_changes` (the
/// `StoreFlagsFailed` rollback path) puts the original flags straight
/// back and clears the stash. Same self-skip convention as the other
/// GTK-touching tests in this module.
#[test]
fn optimistic_toggle_read_and_restore_round_trips_flags_and_unified_snapshot() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    // `summary()` defaults to no flags set, i.e. unread.
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone()], SortKey::Date, true);
    state.borrow_mut().unified_snapshots.insert(mailbox.clone(), vec![one.clone(), two.clone()]);

    optimistic_toggle_read(&state, &message_list, &mailbox, &[Uid(2)], true);
    let patched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    assert!(!patched.is_unread(), "the row must flip to read immediately, before any server round trip");
    let untouched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(1)).unwrap();
    assert!(untouched.is_unread(), "an uid not in the toggle set must be untouched");
    assert!(
        !state.borrow().unified_snapshots[&mailbox].iter().find(|m| m.uid == Uid(2)).unwrap().is_unread(),
        "the unified-view snapshot must reflect the toggle too"
    );
    assert_eq!(
        state.borrow().pending_optimistic_flag_changes[&mailbox].iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![Uid(2)],
        "the pre-toggle summary must be stashed for a possible rollback"
    );

    restore_optimistic_flag_changes(&state, &message_list, &mailbox, &[Uid(2)]);
    let restored = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    assert!(restored.is_unread(), "a failed flag update must restore the original unread state");
    assert!(
        state.borrow().unified_snapshots[&mailbox].iter().find(|m| m.uid == Uid(2)).unwrap().is_unread(),
        "the unified-view snapshot must be restored too"
    );
    assert!(
        !state.borrow().pending_optimistic_flag_changes.contains_key(&mailbox),
        "the stash must be empty once its only entry is restored"
    );
}

/// The pin/unpin sibling of the read-toggle test above:
/// `optimistic_toggle_pinned` flips `SystemFlagBit::Flagged` (and the
/// unified-view snapshot's copy) the instant it's called, stashing the
/// pre-toggle summary in its own `pending_optimistic_pinned_changes`
/// stash rather than the read-toggle's, and a matching
/// `restore_optimistic_pinned_changes` puts the original flag straight
/// back and clears that stash without touching the other one.
#[test]
fn optimistic_toggle_pinned_and_restore_round_trips_flags_and_unified_snapshot() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    // `summary()` defaults to no flags set, i.e. unpinned.
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone()], SortKey::Date, true);
    state.borrow_mut().unified_snapshots.insert(mailbox.clone(), vec![one.clone(), two.clone()]);

    optimistic_toggle_pinned(&state, &message_list, &mailbox, &[Uid(2)], true);
    let patched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    assert!(patched.is_pinned(), "the row must flip to pinned immediately, before any server round trip");
    let untouched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(1)).unwrap();
    assert!(!untouched.is_pinned(), "an uid not in the toggle set must be untouched");
    assert!(
        state.borrow().unified_snapshots[&mailbox].iter().find(|m| m.uid == Uid(2)).unwrap().is_pinned(),
        "the unified-view snapshot must reflect the toggle too"
    );
    assert_eq!(
        state.borrow().pending_optimistic_pinned_changes[&mailbox].iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![Uid(2)],
        "the pre-toggle summary must be stashed for a possible rollback, in its own stash"
    );
    assert!(
        !state.borrow().pending_optimistic_flag_changes.contains_key(&mailbox),
        "a pin toggle must never touch the read-toggle's stash"
    );

    restore_optimistic_pinned_changes(&state, &message_list, &mailbox, &[Uid(2)]);
    let restored = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    assert!(!restored.is_pinned(), "a failed flag update must restore the original unpinned state");
    assert!(
        !state.borrow().unified_snapshots[&mailbox].iter().find(|m| m.uid == Uid(2)).unwrap().is_pinned(),
        "the unified-view snapshot must be restored too"
    );
    assert!(
        !state.borrow().pending_optimistic_pinned_changes.contains_key(&mailbox),
        "the stash must be empty once its only entry is restored"
    );
}

/// Regression test for the disappear-reappear-disappear flicker: the
/// account session's IDLE-vs-command race can land a `MessagesUpdated`
/// snapshot that still shows a just-deleted uid, because the resync ran
/// for an unrelated reason before the queued delete reached the server.
/// `reconcile_optimistic_removals` must keep the row hidden and the
/// stash entry alive in that case, and only clear the stash once a
/// snapshot actually omits the uid.
#[test]
fn reconcile_optimistic_removals_survives_a_stale_racing_sync() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    let three = summary(Uid(3), "acc:INBOX", 2026, 8, 1, 11);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone(), three.clone()], SortKey::Date, true);
    optimistic_remove_messages(&state, &message_list, &mailbox, &[Uid(2)]);

    // A sync that raced ahead of the actual delete still has all three
    // rows - reconciliation must strip the pending uid back out and
    // keep it stashed rather than trust the stale snapshot.
    let mut racing_snapshot = vec![one.clone(), two.clone(), three.clone()];
    reconcile_optimistic_removals(&state, &mailbox, &mut racing_snapshot);
    assert_eq!(
        racing_snapshot.iter().map(|m| m.uid.0).collect::<HashSet<_>>(),
        HashSet::from([1, 3]),
        "a stale racing sync must not be allowed to repaint the deleted row"
    );
    assert_eq!(
        state.borrow().pending_optimistic_removals[&mailbox].iter().map(|m| m.uid.0).collect::<Vec<_>>(),
        vec![2],
        "the stash must survive a sync that hasn't confirmed the delete yet"
    );

    // The genuine post-delete sync omits uid 2 - that's confirmation,
    // so the stash clears and the snapshot passes through untouched.
    let mut confirmed_snapshot = vec![one, three];
    reconcile_optimistic_removals(&state, &mailbox, &mut confirmed_snapshot);
    assert_eq!(confirmed_snapshot.iter().map(|m| m.uid.0).collect::<HashSet<_>>(), HashSet::from([1, 3]));
    assert!(
        !state.borrow().pending_optimistic_removals.contains_key(&mailbox),
        "a sync confirming the uid is gone must clear the stash"
    );
}

/// The read/unread sibling of the delete-flicker regression test: a
/// racing sync can land with the pre-toggle `Seen` flag before the
/// actual `STORE` reaches the server. `reconcile_optimistic_flag_changes`
/// must patch the stale snapshot back to the optimistic value (not let
/// the row flip back to unread) and only clear the stash once a
/// snapshot's flag actually matches.
#[test]
fn reconcile_optimistic_flag_changes_survives_a_stale_racing_sync() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone()], SortKey::Date, true);
    optimistic_toggle_read(&state, &message_list, &mailbox, &[Uid(2)], true);

    // A sync that raced ahead of the actual STORE still shows the
    // pre-toggle unread flag - reconciliation must patch it back to the
    // optimistic (read) value and keep the uid stashed.
    let mut racing_snapshot = vec![one.clone(), two.clone()];
    reconcile_optimistic_flag_changes(&state, &message_list, &mailbox, &mut racing_snapshot);
    assert!(
        !racing_snapshot.iter().find(|m| m.uid == Uid(2)).unwrap().is_unread(),
        "a stale racing sync must not be allowed to flip the row back to unread"
    );
    assert_eq!(
        state.borrow().pending_optimistic_flag_changes[&mailbox].iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![Uid(2)],
        "the stash must survive a sync that hasn't confirmed the flag change yet"
    );

    // The genuine post-STORE sync agrees the row is read - that's
    // confirmation, so the stash clears.
    let two_read = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    let mut confirmed_snapshot = vec![one, two_read];
    reconcile_optimistic_flag_changes(&state, &message_list, &mailbox, &mut confirmed_snapshot);
    assert!(
        !state.borrow().pending_optimistic_flag_changes.contains_key(&mailbox),
        "a sync confirming the flag change must clear the stash"
    );
}

/// The pin/unpin sibling of the test above: a racing sync can land
/// with the pre-toggle `Flagged` state before the actual `STORE` reaches
/// the server. `reconcile_optimistic_pinned_changes` must patch the
/// stale snapshot back to the optimistic value (not let the message drop
/// out of the Pinned group) and only clear its own stash once a
/// snapshot's flag actually matches - independent of the read-toggle
/// stash `reconcile_optimistic_flag_changes` owns.
#[test]
fn reconcile_optimistic_pinned_changes_survives_a_stale_racing_sync() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone()], SortKey::Date, true);
    optimistic_toggle_pinned(&state, &message_list, &mailbox, &[Uid(2)], true);

    // A sync that raced ahead of the actual STORE still shows the
    // pre-toggle unpinned flag - reconciliation must patch it back to
    // the optimistic (pinned) value and keep the uid stashed.
    let mut racing_snapshot = vec![one.clone(), two.clone()];
    reconcile_optimistic_pinned_changes(&state, &message_list, &mailbox, &mut racing_snapshot);
    assert!(
        racing_snapshot.iter().find(|m| m.uid == Uid(2)).unwrap().is_pinned(),
        "a stale racing sync must not be allowed to drop the row's optimistic pin"
    );
    assert_eq!(
        state.borrow().pending_optimistic_pinned_changes[&mailbox].iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![Uid(2)],
        "the stash must survive a sync that hasn't confirmed the flag change yet"
    );

    // The genuine post-STORE sync agrees the row is pinned - that's
    // confirmation, so the stash clears.
    let two_pinned = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    let mut confirmed_snapshot = vec![one, two_pinned];
    reconcile_optimistic_pinned_changes(&state, &message_list, &mailbox, &mut confirmed_snapshot);
    assert!(
        !state.borrow().pending_optimistic_pinned_changes.contains_key(&mailbox),
        "a sync confirming the flag change must clear the stash"
    );
}

/// The rendered `preview` of the row for `uid` in `model`'s flat
/// selection, or `None` if it isn't currently rendered as a message row
/// at all - reads through the actual GTK model rather than `truth`/
/// `all_messages()`, so a regression where `message_row_key` silently
/// dropped `preview` from its fingerprint (making a preview-only change
/// look like a no-op) would still be caught here.
fn rendered_preview(model: &MessageListModel, uid: Uid) -> Option<Option<String>> {
    for i in 0..model.selection.n_items() {
        let row = model.selection.item(i).and_downcast::<gtk::TreeListRow>()?;
        let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let item = boxed.borrow::<MessageItem>();
        if let MessageItem::Message(summary) = &*item {
            if summary.uid == uid {
                return Some(summary.preview.clone());
            }
        }
    }
    None
}

/// `patch_previews` patches only the matching `(mailbox, uid)` rows'
/// `preview` in place, leaves everything else untouched, and the change
/// actually reaches the rendered GTK model - not just `truth` - proving
/// `message_row_key`'s fingerprint diff (which `preview` is one field of)
/// correctly treats a preview-only change as a real change to repaint,
/// not a false no-op.
#[test]
fn patch_previews_updates_the_matching_row_and_leaves_others_untouched() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one.clone(), two.clone()], SortKey::Date, true);
    state.borrow_mut().unified_snapshots.insert(mailbox.clone(), vec![one, two]);

    let previews = HashMap::from([(Uid(2), "new snippet".to_string())]);
    patch_previews(&state, &message_list, &mailbox, &previews, &HashMap::new());

    assert_eq!(
        rendered_preview(&message_list, Uid(2)),
        Some(Some("new snippet".to_string())),
        "the patched row must repaint with the new preview"
    );
    assert_eq!(rendered_preview(&message_list, Uid(1)), Some(None), "an untouched uid must render exactly as before");

    let patched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    assert_eq!(patched.preview.as_deref(), Some("new snippet"));

    // A mention-only patch (M-25) reaches the model too, so the row's `@`
    // glyph and the Mentions filter pick it up.
    patch_previews(&state, &message_list, &mailbox, &HashMap::new(), &HashMap::from([(Uid(1), true)]));
    let mentioned = message_list.all_messages().into_iter().find(|m| m.uid == Uid(1)).unwrap();
    assert!(mentioned.mentions);
    let unmentioned = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
    assert!(!unmentioned.mentions);
}

/// A uid the patch doesn't mention, or one for a mailbox not currently
/// displayed at all, is a safe no-op - no panic, no spurious row, no
/// unrelated change.
#[test]
fn patch_previews_ignores_uids_not_in_the_displayed_list() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let account_id = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let other_mailbox = MailboxId("acc:Archive".into());
    let state = test_state(vec![(account_id, Vec::new())]);
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);

    let message_list = MessageListModel::build();
    message_list.repopulate(vec![one], SortKey::Date, true);

    // A uid that doesn't exist in this mailbox, and a whole mailbox
    // that isn't displayed.
    let previews = HashMap::from([(Uid(99), "nobody".to_string())]);
    patch_previews(&state, &message_list, &mailbox, &previews, &HashMap::new());
    assert_eq!(message_list.all_messages().len(), 1, "no row should be added");
    assert_eq!(rendered_preview(&message_list, Uid(1)), Some(None), "the one real row must be untouched");

    let previews_for_other = HashMap::from([(Uid(1), "wrong mailbox".to_string())]);
    patch_previews(&state, &message_list, &other_mailbox, &previews_for_other, &HashMap::new());
    assert_eq!(
        rendered_preview(&message_list, Uid(1)),
        Some(None),
        "a preview for a different mailbox must not leak onto this uid"
    );
}

/// The depth-0 row for one account group in a freshly built tree.
fn account_row(model: &gtk::TreeListModel, id: &AccountId) -> gtk::TreeListRow {
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
        if row.depth() != 0 {
            continue;
        }
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
        let tree_item = boxed.borrow::<TreeItem>();
        if let TreeItem::Account(acc) = &*tree_item {
            if &acc.account_id == id {
                return row;
            }
        }
    }
    panic!("no depth-0 account row for {id:?}");
}

/// The depth-0 Favorites section row of a freshly built tree.
fn favorites_row(model: &gtk::TreeListModel) -> gtk::TreeListRow {
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
        if row.depth() != 0 {
            continue;
        }
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
        let tree_item = boxed.borrow::<TreeItem>();
        if matches!(&*tree_item, TreeItem::Favorites) {
            return row;
        }
    }
    panic!("no Favorites row");
}

/// The mailbox the folder pane's selection currently highlights, if any.
fn selected_mailbox(selection: &gtk::SingleSelection) -> Option<MailboxId> {
    let row = selection.selected_item().and_downcast::<gtk::TreeListRow>()?;
    let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let tree_item = boxed.borrow::<TreeItem>();
    match &*tree_item {
        TreeItem::Folder(node) => Some(node.mailbox.id.clone()),
        _ => None,
    }
}

/// The folder-pane collapse regression tests, in one `#[test]` for the
/// same reason `MessageListModel`'s GTK suite is: GTK may only be
/// initialised on a single thread, and libtest gives each `#[test]` its
/// own - so several GTK-touching tests race to `gtk::init()` and
/// whichever loses panics. Skipped when the host has no display to
/// initialise against. Covers: (1) a user's manual account-group collapse
/// surviving the constant `FoldersUpdated` rebuilds (with the Favorites
/// section's own collapse riding along under its reserved key), (2) a
/// slow-connecting account rendering *expandable but empty* rather than a
/// chevron-less leaf and popping open when its folders land, (3) the
/// end-to-end `rebuild_folder_tree` path preserving collapse, selection,
/// and scroll, and (4) user-expanded subfolders (2nd tier and deeper)
/// surviving the same rebuilds.
#[test]
fn folder_tree_rebuild_preserves_collapse_state_and_scroll() {
    // GTK's display-dependent tests can only run when the host has a
    // display AND this test's thread is the one that initialized GTK -
    // `gtk::init()` panics if another test thread got there first, so the
    // suite skips instead of failing (same self-skipping convention as
    // `theme.rs`'s `gtk_ok`).
    if !crate::gtk_test::gtk_ready() {
        return;
    }

    let acc_a = AccountId("acc_a".into());
    let acc_b = AccountId("acc_b".into());

    // --- (1) A user's account-group collapse survives a rebuild ---
    let build = || {
        build_multi_account_tree_model(
            vec![
                (acc_a.clone(), "A".into(), vec![test_mailbox(&acc_a, "INBOX", 3)]),
                (acc_b.clone(), "B".into(), vec![test_mailbox(&acc_b, "INBOX", 0)]),
            ],
            vec![test_mailbox(&acc_a, "INBOX", 3)],
        )
    };
    let model = build();
    apply_account_group_expansion(&model, &HashSet::new());
    assert!(account_row(&model, &acc_a).is_expanded(), "default: every account group expanded");
    assert!(account_row(&model, &acc_b).is_expanded());
    assert!(favorites_row(&model).is_expanded());

    // The user collapses account B and the Favorites section.
    account_row(&model, &acc_b).set_expanded(false);
    favorites_row(&model).set_expanded(false);

    let selection = gtk::SingleSelection::new(Some(model));
    let collapsed = collapsed_account_groups(&selection);
    assert!(collapsed.contains(&acc_b));
    assert!(collapsed.contains(&AccountId(FAVORITES_GROUP_KEY.into())));
    assert!(!collapsed.contains(&acc_a), "account A was left expanded");

    // A rebuild (e.g. a count refresh) must put the pane back the way the
    // user left it, not revert everything to all-expanded.
    let rebuilt = build();
    apply_account_group_expansion(&rebuilt, &collapsed);
    assert!(account_row(&rebuilt, &acc_a).is_expanded());
    assert!(!account_row(&rebuilt, &acc_b).is_expanded(), "account B's collapse survived the rebuild");
    assert!(!favorites_row(&rebuilt).is_expanded(), "the Favorites collapse survived too");

    // --- (2) A not-yet-connected account stays expandable-in-waiting ---
    let waiting = build_multi_account_tree_model(
        vec![(acc_a.clone(), "A".into(), vec![test_mailbox(&acc_a, "INBOX", 3)]), (acc_b.clone(), "B".into(), Vec::new())],
        vec![],
    );
    apply_account_group_expansion(&waiting, &HashSet::new());
    let b_row = account_row(&waiting, &acc_b);
    assert!(b_row.is_expandable(), "a not-yet-connected account must not render as a leaf");
    assert!(b_row.is_expanded(), "it defaults to expanded like every account group");

    let selection = gtk::SingleSelection::new(Some(waiting));
    let collapsed = collapsed_account_groups(&selection);
    assert!(!collapsed.contains(&acc_b), "an account still waiting for folders can't be user-collapsed");

    // Its folders arrive on the next sync: the rebuild re-opens it.
    let connected = build_multi_account_tree_model(
        vec![
            (acc_a.clone(), "A".into(), vec![test_mailbox(&acc_a, "INBOX", 4)]),
            (acc_b.clone(), "B".into(), vec![test_mailbox(&acc_b, "INBOX", 1)]),
        ],
        vec![],
    );
    apply_account_group_expansion(&connected, &collapsed);
    let b_row = account_row(&connected, &acc_b);
    assert!(b_row.is_expandable());
    assert!(b_row.is_expanded(), "the account pops open as soon as its folders land");
    assert!(b_row.child_row(0).is_some(), "its INBOX is reachable beneath it");

    // --- (3) End to end through `rebuild_folder_tree` ---
    let state = test_state(vec![
        (acc_a.clone(), vec![test_mailbox(&acc_a, "INBOX", 3)]),
        (acc_b.clone(), vec![test_mailbox(&acc_b, "INBOX", 0)]),
    ]);
    state.borrow_mut().current_account = Some(acc_a.clone());
    state.borrow_mut().current_mailbox = Some(MailboxId("acc_a:INBOX".into()));

    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_scroller = gtk::ScrolledWindow::new();
    folder_scroller.set_vadjustment(Some(&gtk::Adjustment::new(0.0, 0.0, 100.0, 0.0, 0.0, 0.0)));
    let adjustment = folder_scroller.vadjustment();
    adjustment.set_value(42.0);

    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    assert!(account_row(&model, &acc_a).is_expanded());
    assert!(account_row(&model, &acc_b).is_expanded());
    assert_eq!(
        selected_mailbox(&folder_selection),
        Some(MailboxId("acc_a:INBOX".into())),
        "first build restores the open mailbox"
    );

    // The user collapses account B.
    account_row(&model, &acc_b).set_expanded(false);

    // A count refresh lands: the signature changes, so the rebuild runs.
    state.borrow_mut().accounts.get_mut(&acc_b).unwrap().folders[0].unread = 5;
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);

    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    assert!(account_row(&model, &acc_a).is_expanded(), "account A stays expanded");
    assert!(!account_row(&model, &acc_b).is_expanded(), "account B's collapse survives the rebuild");
    assert_eq!(selected_mailbox(&folder_selection), Some(MailboxId("acc_a:INBOX".into())), "the selection is restored");
    assert_eq!(adjustment.value(), 42.0, "the scroll position survives the model swap");

    // --- (4) Expanded subfolders (2nd tier and deeper) survive a rebuild ---
    let state = test_state(vec![(
        acc_a.clone(),
        vec![
            test_mailbox(&acc_a, "INBOX", 0),
            test_mailbox(&acc_a, "Projects", 0),
            test_mailbox(&acc_a, "Projects/Active", 0),
            test_mailbox(&acc_a, "Projects/Active/Team", 0),
        ],
    )]);
    state.borrow_mut().current_account = Some(acc_a.clone());
    state.borrow_mut().current_mailbox = Some(MailboxId("acc_a:INBOX".into()));

    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_scroller = gtk::ScrolledWindow::new();
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    // The user expands "Projects" and, beneath it, "Projects/Active".
    let folder_row = |model: &gtk::TreeListModel, id: &str| {
        let idx = find_mailbox_index(model, &MailboxId(id.into())).expect("folder row");
        model.item(idx).and_downcast::<gtk::TreeListRow>().expect("TreeListRow")
    };
    folder_row(&model, "acc_a:Projects").set_expanded(true);
    folder_row(&model, "acc_a:Projects/Active").set_expanded(true);
    folder_row(&model, "acc_a:Projects/Active/Team");

    // A count refresh lands: the signature changes, so the rebuild runs.
    state.borrow_mut().accounts.get_mut(&acc_a).unwrap().folders[0].unread = 7;
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);

    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    assert!(folder_row(&model, "acc_a:Projects").is_expanded(), "an expanded top-level folder survives a rebuild");
    assert!(
        folder_row(&model, "acc_a:Projects/Active").is_expanded(),
        "an expanded 2nd-tier subfolder survives a rebuild"
    );
    folder_row(&model, "acc_a:Projects/Active/Team"); // still materialized under its expanded parent

    // --- (5) A fresh session (restart) re-seeds the first build from the
    // persisted expansion memory, which is exactly what survives a restart ---
    let state = test_state(vec![
        (
            acc_a.clone(),
            vec![
                test_mailbox(&acc_a, "INBOX", 0),
                test_mailbox(&acc_a, "Projects", 0),
                test_mailbox(&acc_a, "Projects/Active", 0),
                test_mailbox(&acc_a, "Projects/Active/Team", 0),
            ],
        ),
        (acc_b.clone(), vec![test_mailbox(&acc_b, "INBOX", 0)]),
    ]);
    state.borrow_mut().current_account = Some(acc_a.clone());
    state.borrow_mut().current_mailbox = Some(MailboxId("acc_a:INBOX".into()));
    // The last session left "Projects" expanded, "Projects/Active" too,
    // and account B collapsed - the persisted `UiState` fields a startup
    // `build_window` fills from the UI-state database.
    state.borrow_mut().expanded_folders = HashSet::from([MailboxId("acc_a:Projects".into()), MailboxId("acc_a:Projects/Active".into())]);
    state.borrow_mut().collapsed_groups = HashSet::from([acc_b.clone()]);

    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_scroller = gtk::ScrolledWindow::new();
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);

    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    assert!(
        account_row(&model, &acc_a).is_expanded(),
        "an account group the last session left expanded stays expanded on the first build"
    );
    assert!(
        !account_row(&model, &acc_b).is_expanded(),
        "an account group the last session left collapsed stays collapsed on the first build"
    );
    assert!(
        folder_row(&model, "acc_a:Projects").is_expanded(),
        "a persisted expanded subfolder opens on the first build"
    );
    assert!(folder_row(&model, "acc_a:Projects/Active").is_expanded(), "a persisted 2nd-tier subfolder opens too");
    folder_row(&model, "acc_a:Projects/Active/Team"); // materialized under its restored expanded parents

    // --- (6) The real startup order: the first builds run before any
    // account has connected, so the pane is empty and can say nothing
    // about what the user expanded. That silence must not be recorded as
    // "nothing is expanded" - doing so wiped the restored memory within
    // milliseconds of launch, and the expansion state never survived a
    // restart even though it had been written correctly on exit ---
    let state = test_state(Vec::new());
    state.borrow_mut().expanded_folders = HashSet::from([MailboxId("acc_a:Projects".into()), MailboxId("acc_a:Projects/Active".into())]);
    state.borrow_mut().collapsed_groups = HashSet::from([acc_b.clone()]);
    let remembered_folders = state.borrow().expanded_folders.clone();
    let remembered_groups = state.borrow().collapsed_groups.clone();

    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_scroller = gtk::ScrolledWindow::new();
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    // A second empty build (the pane rebuilds on every `FoldersUpdated`)
    // now has a live - but still accountless - model to capture from.
    state.borrow_mut().folder_tree = None;
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    assert_eq!(
        state.borrow().expanded_folders,
        remembered_folders,
        "an accountless pane must not wipe the expanded-folder memory"
    );
    assert_eq!(
        state.borrow().collapsed_groups,
        remembered_groups,
        "an accountless pane must not wipe the collapsed-group memory"
    );

    // The accounts connect and their folders land: the memory the empty
    // builds preserved is what the pane opens with.
    let arrived = test_state(vec![
        (
            acc_a.clone(),
            vec![
                test_mailbox(&acc_a, "INBOX", 0),
                test_mailbox(&acc_a, "Projects", 0),
                test_mailbox(&acc_a, "Projects/Active", 0),
                test_mailbox(&acc_a, "Projects/Active/Team", 0),
            ],
        ),
        (acc_b.clone(), vec![test_mailbox(&acc_b, "INBOX", 0)]),
    ]);
    state.borrow_mut().accounts = std::mem::take(&mut arrived.borrow_mut().accounts);
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);

    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    assert!(
        !account_row(&model, &acc_b).is_expanded(),
        "the remembered account-group collapse applies once the account connects"
    );
    assert!(
        folder_row(&model, "acc_a:Projects").is_expanded(),
        "the remembered subfolder expansion applies once the folders land"
    );
    assert!(folder_row(&model, "acc_a:Projects/Active").is_expanded(), "and so does the remembered 2nd-tier one");

    // Collapsing is still recorded: the memory only ignores rows the pane
    // couldn't speak for, never a row the user actually toggled.
    folder_row(&model, "acc_a:Projects/Active").set_expanded(false);
    state.borrow_mut().folder_tree = None;
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    assert_eq!(
        state.borrow().expanded_folders,
        HashSet::from([MailboxId("acc_a:Projects".into())]),
        "a collapsed subfolder is dropped from the memory"
    );

    // Collapsing the account group hides its subfolder rows entirely, so
    // the capture can't see them any more - and must therefore leave
    // their remembered state alone. Otherwise collapsing an account would
    // silently forget every subfolder the user had opened inside it.
    let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
    account_row(&model, &acc_a).set_expanded(false);
    state.borrow_mut().folder_tree = None;
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    assert_eq!(
        state.borrow().collapsed_groups,
        HashSet::from([acc_a.clone(), acc_b.clone()]),
        "a collapsed account group is added to the memory"
    );
    assert_eq!(
        state.borrow().expanded_folders,
        HashSet::from([MailboxId("acc_a:Projects".into())]),
        "the subfolders hidden by that collapse keep their remembered expansion"
    );
}

/// The folder-expansion flush hangs off `Application::shutdown` because
/// the real exit paths (Ctrl+Q, File → Quit, the tray's Quit) call
/// `app.quit()` and never run the window's close-request handler. That
/// only works if `shutdown` is emitted while the folder tree's model is
/// still alive - GTK's default handler closes the windows *after* the
/// signal - so this pins the ordering the flush depends on. Like the
/// other GTK-touching tests it self-skips when this thread doesn't own
/// GTK (see `gtk_test::gtk_ready`).
#[test]
fn shutdown_signal_fires_with_tree_still_alive() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }
    let app = adw::Application::builder().application_id("org.example.lookout-shutdown-order-test").build();
    let win = gtk::ApplicationWindow::new(&app);
    let model = build_multi_account_tree_model(Vec::new(), Vec::new());
    let selection = gtk::SingleSelection::new(Some(model));
    let seen: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
    let seen2 = seen.clone();
    app.connect_shutdown(move |_| {
        *seen2.borrow_mut() = Some(selection.model().is_some());
    });
    app.connect_activate({
        let app = app.clone();
        move |_| app.quit()
    });
    // Empty argv: `run()` would hand GApplication libtest's own command
    // line (`--nocapture`, a filter string) and fail to parse it.
    app.run_with_args::<&str>(&[]);
    let seen = *seen.borrow();
    win.close();
    assert_eq!(seen, Some(true), "the tree model must still be alive when shutdown fires");
}

fn summary(uid: Uid, mailbox: &str, year: i32, month: u32, day: u32, hour: u32) -> EmailSummary {
    use chrono::TimeZone;
    use lookout_core::ThreadKey;
    use std::collections::BTreeSet;
    EmailSummary {
        inference_classification: Default::default(),
        uid,
        mailbox: MailboxId(mailbox.into()),
        message_id: None,
        in_reply_to: None,
        references: vec![],
        thread_key: ThreadKey("t".into()),
        subject: None,
        from: vec![],
        to: vec![],
        cc: vec![],
        date: chrono::Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap(),
        flags: BTreeSet::new(),
        keywords: BTreeSet::new(),
        size: 0,
        has_attachment: false,
        has_calendar: false,
        preview: None,
        mentions: false,
        importance: Default::default(),
        sensitivity: Default::default(),
        list_id: None,
        structure: None,
    }
}

fn occ(uid: &str, calendar: &str) -> EventOccurrence {
    EventOccurrence {
        uid: lookout_core::EventUid(uid.to_string()),
        calendar_id: CalendarId(calendar.to_string()),
        summary: Some(uid.to_string()),
        description: None,
        location: None,
        start: chrono::Utc::now(),
        end: chrono::Utc::now(),
        all_day: false,
        time_zone: None,
        rrule: None,
        recurrence_id: None,
        exdates: Vec::new(),
        master_start: None,
        master_end: None,
        href: None,
        etag: None,
        master_href: None,
        master_etag: None,
        sequence: 0,
        attendees: vec![],
        organizer: None,
        schedule_agent: None,
        categories: vec![],
        sensitivity: Default::default(),
        show_as: Default::default(),
        reminder_minutes_before: None,
        conference_url: None,
    }
}

/// The per-month occurrence map must never grow beyond the upcoming
/// window (current month through three months ahead - the months the
/// 90-day upcoming-events horizon needs), or a long-lived session would
/// accumulate stale month buckets (and a too-narrow window is exactly
/// the bug that made the upcoming-events section drain over time).
#[test]
fn dashboard_occurrence_map_prunes_to_the_upcoming_months() {
    let window = dashboard_month_window();
    assert_eq!(window[0], first_of_month(window[0]), "the window is month-normalized");
    for (i, month) in window.iter().enumerate() {
        assert_eq!(
            *month,
            window[0] + chrono::Months::new(i as u32),
            "the window spans the current month through three months ahead"
        );
    }
    let stale_before = window[0] - chrono::Months::new(1);
    let stale_after = window[3] + chrono::Months::new(1);

    let mut map = HashMap::new();
    insert_dashboard_occurrences(&mut map, stale_before, vec![occ("stale", "cal")]);
    assert!(map.is_empty(), "a stale month is pruned immediately");
    for month in window {
        insert_dashboard_occurrences(&mut map, month, vec![occ("now", "cal")]);
    }
    assert_eq!(map.len(), 4, "only the upcoming window's months survive");
    for month in window {
        assert_eq!(map[&month].len(), 1);
    }

    // A re-insert of an out-of-window month evicts itself but keeps the
    // in-window months (and their events) intact.
    insert_dashboard_occurrences(&mut map, stale_after, vec![occ("further", "cal")]);
    assert_eq!(map.len(), 4);
    for month in window {
        assert!(map.contains_key(&month));
    }
}

/// A pending drag-move is reapplied onto the incoming occurrence's
/// start/end for as long as the incoming data doesn't yet reflect it -
/// covers both the immediate post-drop repaint (still showing the
/// pre-drag time) and a `sync_month` cache-hit resync that re-broadcasts
/// stale pre-edit data before the live fetch lands.
#[test]
fn pending_calendar_move_is_reapplied_until_the_incoming_data_confirms_it() {
    let t0 = chrono::Utc::now();
    let t1 = t0 + chrono::Duration::hours(1);
    let mut moved = occ("evt-1", "cal");
    moved.start = t1;
    moved.end = t1 + chrono::Duration::hours(1);
    let mut pending = HashMap::from([((moved.uid.clone(), moved.recurrence_id), moved.clone())]);

    // Still-stale incoming data (the original pre-drag time) gets
    // overwritten to the pending value.
    let mut stale = occ("evt-1", "cal");
    stale.start = t0;
    stale.end = t0 + chrono::Duration::hours(1);
    let mut incoming = vec![stale];
    apply_pending_calendar_moves(&mut incoming, &mut pending);
    assert_eq!(incoming[0].start, t1, "the chip must keep showing the dropped time, not the stale cached one");
    assert!(
        pending.contains_key(&(EventUid("evt-1".into()), None)),
        "not yet confirmed by the server - the entry survives"
    );
}

#[test]
fn pending_calendar_move_self_clears_once_the_server_confirms_it() {
    let t1 = chrono::Utc::now();
    let mut moved = occ("evt-1", "cal");
    moved.start = t1;
    moved.end = t1 + chrono::Duration::hours(1);
    let mut pending = HashMap::from([((moved.uid.clone(), moved.recurrence_id), moved.clone())]);

    // The live fetch's own data already matches what we optimistically
    // set - the entry must be dropped rather than kept reapplying.
    let mut incoming = vec![moved.clone()];
    apply_pending_calendar_moves(&mut incoming, &mut pending);
    assert_eq!(incoming[0].start, t1);
    assert!(pending.is_empty(), "a confirmed move must clear the stash");
}

#[test]
fn pending_calendar_move_leaves_unrelated_occurrences_and_empty_pending_untouched() {
    let t0 = chrono::Utc::now();
    let mut other = occ("evt-2", "cal");
    other.start = t0;
    other.end = t0 + chrono::Duration::hours(1);
    let mut incoming = vec![other.clone()];

    // No pending entry for this occurrence's uid at all.
    let mut pending = HashMap::new();
    apply_pending_calendar_moves(&mut incoming, &mut pending);
    assert_eq!(incoming[0].start, t0, "an occurrence with nothing pending must be untouched");

    // A pending entry that belongs to a different occurrence entirely
    // must not affect this one.
    let mut moved = occ("evt-1", "cal");
    moved.start = t0 + chrono::Duration::hours(5);
    moved.end = moved.start + chrono::Duration::hours(1);
    pending.insert((moved.uid.clone(), moved.recurrence_id), moved);
    apply_pending_calendar_moves(&mut incoming, &mut pending);
    assert_eq!(incoming[0].start, t0);
    assert_eq!(pending.len(), 1, "the unrelated pending entry is untouched too - nothing in the incoming list matched it");
}

/// A folder row's drop must reach the account session as one
/// `MoveMessagesTo` per source mailbox, keeping only the dragged
/// messages that belong to the target folder's own account - the whole
/// server side of drag-a-message-onto-a-folder, minus the GTK plumbing
/// (which is exercised manually). Regression guard for the drop path:
/// the payload arrives as a `G_TYPE_BYTES` value holding the JSON
/// `(mailbox, uid)` list.
#[test]
fn folder_drop_moves_payload_messages_to_the_target_folder() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 3), test_mailbox(&acc, "Archive", 0)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;

    // Two messages from the target's account, one from a different one.
    let payload = serde_json::to_vec(&vec![("acc:INBOX".to_string(), 7u32), ("acc:INBOX".to_string(), 9u32), ("other:INBOX".to_string(), 1u32)]).unwrap();
    let value = glib::Value::from(glib::Bytes::from(payload.as_slice()));
    let target = test_mailbox(&acc, "Archive", 0);

    let handled = handle_message_drag_drop(&state, &target, &value);
    assert!(handled, "a well-formed payload must be claimed by the folder's drop target");
    match cmd_rx.try_recv() {
        Ok(AccountCommand::MoveMessagesTo {
            mailbox,
            uids,
            target: got_target,
        }) => {
            assert_eq!(mailbox, MailboxId("acc:INBOX".into()));
            assert_eq!(uids, vec![Uid(7), Uid(9)], "the other account's message is refused");
            assert_eq!(got_target, MailboxId("acc:Archive".into()));
        }
        other => panic!("expected MoveMessagesTo, got {other:?}"),
    }
    assert!(cmd_rx.try_recv().is_err(), "cross-account messages are dropped, so only one command goes out");

    // Its Undo puts them back by `RevertMove`, which resolves the UIDs the
    // move gave them in the target (item 1.13) - not a move of the source
    // UIDs, which mean nothing in the target folder.
    assert!(undo_last(&state));
    match cmd_rx.try_recv() {
        Ok(AccountCommand::RevertMove { moved_from, moved_to, uids }) => {
            assert_eq!(moved_from, MailboxId("acc:INBOX".into()));
            assert_eq!(moved_to, MailboxId("acc:Archive".into()));
            assert_eq!(uids, vec![Uid(7), Uid(9)]);
        }
        other => panic!("expected RevertMove, got {other:?}"),
    }
}

/// Anything that isn't the message-drag payload must be declined so GTK
/// can pass the drop on to another target - a foreign app's bytes won't
/// parse as the JSON shape, and a non-bytes value (e.g. a text drop)
/// isn't ours at all.
#[test]
fn folder_drop_rejects_foreign_payloads() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 0)])]);
    let target = test_mailbox(&acc, "Archive", 0);

    assert!(!handle_message_drag_drop(&state, &target, &glib::Value::from("hello")), "a text drop is not our payload");
    assert!(
        !handle_message_drag_drop(&state, &target, &glib::Value::from(glib::Bytes::from_static(b"not json"))),
        "junk bytes are not our payload"
    );
}

/// The tag rows' drop side applies the tag to every dragged message of
/// that account, via one `StoreKeywordsMany` per source mailbox - the
/// batch counterpart of the Categorize menu's toggles.
#[test]
fn tag_drop_stores_the_keyword_on_payload_messages() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 3)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;

    let payload = serde_json::to_vec(&vec![("acc:INBOX".to_string(), 7u32)]).unwrap();
    let value = glib::Value::from(glib::Bytes::from(payload.as_slice()));
    assert!(handle_keyword_drag_drop(&state, "work", &value));
    match cmd_rx.try_recv() {
        Ok(AccountCommand::StoreKeywordsMany { mailbox, uids, add, remove }) => {
            assert_eq!(mailbox, MailboxId("acc:INBOX".into()));
            assert_eq!(uids, vec![Uid(7)]);
            assert_eq!(add, vec![lookout_core::tag_keyword("work")]);
            assert!(remove.is_empty());
        }
        other => panic!("expected StoreKeywordsMany, got {other:?}"),
    }
    assert!(cmd_rx.try_recv().is_err(), "only one command goes out");
}

/// `record_undo` + Ctrl+Z (`undo_last`) replay the inverse command to the
/// account that owns its mailbox, then consume the entry.
#[test]
fn undo_last_replays_the_inverse_to_the_owning_account() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;

    record_undo(
        &state,
        "1 item deleted",
        vec![AccountCommand::RevertMove {
            moved_from: MailboxId("acc:INBOX".into()),
            moved_to: MailboxId("acc:Trash".into()),
            uids: vec![Uid(7)],
        }],
    );
    assert_eq!(state.borrow().undo_stack.len(), 1);

    assert!(undo_last(&state));
    match cmd_rx.try_recv() {
        Ok(AccountCommand::RevertMove { moved_from, moved_to, uids }) => {
            assert_eq!(moved_from, MailboxId("acc:INBOX".into()));
            assert_eq!(moved_to, MailboxId("acc:Trash".into()));
            assert_eq!(uids, vec![Uid(7)]);
        }
        other => panic!("expected RevertMove, got {other:?}"),
    }
    assert!(state.borrow().undo_stack.is_empty(), "Ctrl+Z consumes the entry");
    assert!(!undo_last(&state), "nothing left to undo");
}

/// Undo snooze routes `UnsnoozeMessages` to the account owning the
/// snoozed messages' mailbox.
#[test]
fn undo_snooze_routes_unsnooze_to_the_owning_account() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;

    record_undo(
        &state,
        "Snoozed until tomorrow 9:00 AM",
        vec![AccountCommand::UnsnoozeMessages {
            mailbox: MailboxId("acc:INBOX".into()),
            uids: vec![Uid(3)],
        }],
    );
    assert!(undo_last(&state));
    assert!(matches!(cmd_rx.try_recv(), Ok(AccountCommand::UnsnoozeMessages { uids, .. }) if uids == vec![Uid(3)]));
}

/// Flagging (item 1.16) shows the flag at once, sends one `SetFollowUp`
/// for the mailbox, and its Undo restores each message's own earlier
/// state - one inverse per previous state, routed to the owning account.
#[test]
fn set_follow_up_shows_sends_and_undoes_per_previous_state() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }
    let acc = AccountId("acc".into());
    let mailbox = MailboxId("acc:INBOX".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;
    let plain = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let mut done = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    done.keywords.extend(lookout_core::follow_up_keywords(&crate::follow_up::completed()));
    let message_list = MessageListModel::build();
    message_list.repopulate(vec![plain.clone(), done.clone()], SortKey::Date, true);

    let due = crate::follow_up::flagged(chrono::NaiveDate::from_ymd_opt(2026, 10, 16));
    set_follow_up(&state, &message_list, vec![plain, done], due.clone());
    assert!(
        message_list.all_messages().iter().all(|m| m.follow_up() == Some(due.clone())),
        "shown before the server answers"
    );
    match cmd_rx.try_recv() {
        Ok(AccountCommand::SetFollowUp { mailbox: sent, mut uids, flag }) => {
            uids.sort();
            assert_eq!((sent, uids, flag), (mailbox.clone(), vec![Uid(1), Uid(2)], due));
        }
        other => panic!("expected SetFollowUp, got {other:?}"),
    }

    assert!(undo_last(&state));
    let mut restored: Vec<(Vec<Uid>, lookout_core::FlagStatus)> = std::iter::from_fn(|| cmd_rx.try_recv().ok())
        .map(|command| match command {
            AccountCommand::SetFollowUp { uids, flag, .. } => (uids, flag.status),
            other => panic!("expected SetFollowUp, got {other:?}"),
        })
        .collect();
    restored.sort_by_key(|(uids, _)| uids[0]);
    assert_eq!(
        restored,
        vec![(vec![Uid(1)], lookout_core::FlagStatus::NotFlagged), (vec![Uid(2)], lookout_core::FlagStatus::Complete)]
    );
}

/// Setting importance (M-12) shows it at once, sends one `SetImportance` for
/// the mailbox, and its Undo restores each message's own earlier importance.
#[test]
fn set_importance_shows_sends_and_undoes_per_previous_state() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;
    let mut high = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    high.importance = lookout_core::Importance::High;
    let plain = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    let message_list = MessageListModel::build();
    message_list.repopulate(vec![high, plain], SortKey::Date, true);

    let targets = message_list.all_messages();
    set_importance(&state, &message_list, &targets, lookout_core::Importance::Low);
    assert!(
        message_list.all_messages().iter().all(|m| m.importance == lookout_core::Importance::Low),
        "shown before the server answers"
    );
    match cmd_rx.try_recv() {
        Ok(AccountCommand::SetImportance { mut uids, importance, .. }) => {
            uids.sort();
            assert_eq!((uids, importance), (vec![Uid(1), Uid(2)], lookout_core::Importance::Low));
        }
        other => panic!("expected SetImportance, got {other:?}"),
    }

    assert!(undo_last(&state));
    let mut restored: Vec<(Vec<Uid>, lookout_core::Importance)> = std::iter::from_fn(|| cmd_rx.try_recv().ok())
        .map(|command| match command {
            AccountCommand::SetImportance { uids, importance, .. } => (uids, importance),
            other => panic!("expected SetImportance, got {other:?}"),
        })
        .collect();
    restored.sort_by_key(|(uids, _)| uids[0]);
    assert_eq!(
        restored,
        vec![(vec![Uid(1)], lookout_core::Importance::High), (vec![Uid(2)], lookout_core::Importance::Normal)]
    );
}

/// Reclassifying Focused/Other (M-03) shows it at once, sends one `SetFocused`
/// for the mailbox, and its Undo restores each message's previous bucket.
#[test]
fn set_focused_shows_sends_and_undoes_per_previous_state() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;
    let mut other = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    other.inference_classification = lookout_core::InferenceClassification::Other;
    let focused = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    let message_list = MessageListModel::build();
    message_list.repopulate(vec![other, focused], SortKey::Date, true);

    let targets = message_list.all_messages();
    set_focused(
        &state,
        &message_list,
        &targets,
        lookout_core::InferenceClassification::Focused,
        lookout_mail::session::FocusedScope::Message,
    );
    assert!(
        message_list
            .all_messages()
            .iter()
            .all(|m| m.inference_classification == lookout_core::InferenceClassification::Focused),
        "shown before the server answers"
    );
    match cmd_rx.try_recv() {
        Ok(AccountCommand::SetFocused {
            mut uids, classification, scope, ..
        }) => {
            uids.sort();
            assert_eq!((uids, classification), (vec![Uid(1), Uid(2)], lookout_core::InferenceClassification::Focused));
            assert_eq!(scope, lookout_mail::session::FocusedScope::Message);
        }
        other => panic!("expected SetFocused, got {other:?}"),
    }

    assert!(undo_last(&state));
    let mut restored: Vec<(Vec<Uid>, lookout_core::InferenceClassification)> = std::iter::from_fn(|| cmd_rx.try_recv().ok())
        .map(|command| match command {
            AccountCommand::SetFocused { uids, classification, .. } => (uids, classification),
            other => panic!("expected SetFocused, got {other:?}"),
        })
        .collect();
    restored.sort_by_key(|(uids, _)| uids[0]);
    assert_eq!(
        restored,
        vec![
            (vec![Uid(1)], lookout_core::InferenceClassification::Other),
            (vec![Uid(2)], lookout_core::InferenceClassification::Focused)
        ]
    );
}

/// A toast's own Undo button removes exactly its entry, leaving later
/// actions for Ctrl+Z.
#[test]
fn take_undo_removes_only_the_named_entry() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let flag = |uid: u32| AccountCommand::StoreFlagsMany {
        mailbox: MailboxId("acc:INBOX".into()),
        uids: vec![Uid(uid)],
        add: Vec::new(),
        remove: Vec::new(),
    };
    record_undo(&state, "first", vec![flag(1)]);
    let first = state.borrow().undo_stack.last().unwrap().id;
    record_undo(&state, "second", vec![flag(2)]);

    let taken = take_undo(&state, first).expect("first is still on the stack");
    assert_eq!(taken.description, "first");
    assert_eq!(state.borrow().undo_stack.len(), 1);
    assert_eq!(state.borrow().undo_stack[0].description, "second");
    assert!(take_undo(&state, first).is_none(), "taking it again is a no-op");
}

#[test]
fn undo_move_descriptions_match_outlook_wording() {
    assert_eq!(undo_move_description(MailboxRole::Trash, 1), "1 item deleted");
    assert_eq!(undo_move_description(MailboxRole::Trash, 2), "2 items deleted");
    assert_eq!(undo_move_description(MailboxRole::Archive, 1), "1 item archived");
    assert_eq!(undo_move_description(MailboxRole::Junk, 3), "3 items reported as junk");
}

/// Coalescing is the point of the bounded event channels: the startup
/// burst queues the same whole-folder snapshot several times in a row
/// (cache replay, live sync, previews), and only the last copy of each
/// snapshot must survive the drain - the UI repaints once per batch, not
/// once per queued copy.
#[test]
fn collapse_account_events_keeps_only_the_last_copy_of_each_snapshot() {
    let a = MailboxId("a:INBOX".into());
    let inbox = |unread: u32| vec![test_mailbox(&AccountId("a".into()), "INBOX", unread)];
    let collapsed = collapse_account_events(vec![
        AccountEvent::FoldersUpdated(inbox(1)),
        AccountEvent::MessagesUpdated {
            mailbox: a.clone(),
            messages: Vec::new(),
        },
        AccountEvent::FoldersUpdated(inbox(2)),
    ]);
    assert_eq!(collapsed.len(), 2, "the earlier folder list is superseded");
    assert!(matches!(&collapsed[0], AccountEvent::MessagesUpdated { mailbox, .. } if mailbox == &a));
    let AccountEvent::FoldersUpdated(folders) = &collapsed[1] else {
        panic!("expected the surviving FoldersUpdated last, got {collapsed:?}");
    };
    assert_eq!(folders[0].unread, 2, "only the newest folder list survives, in its original position");
}

#[test]
fn collapse_account_events_keeps_the_last_messages_updated_per_mailbox_in_order() {
    let a = MailboxId("a:INBOX".into());
    let b = MailboxId("a:Archive".into());
    let msg = |m: MailboxId| AccountEvent::MessagesUpdated { mailbox: m, messages: Vec::new() };
    let collapsed = collapse_account_events(vec![
        AccountEvent::NewMessages {
            mailbox: a.clone(),
            messages: Vec::new(),
        },
        msg(a.clone()),
        msg(b.clone()),
        msg(a.clone()),
    ]);
    assert_eq!(collapsed.len(), 3, "the superseded first copy of A is dropped, the rest stay");
    assert!(matches!(&collapsed[0], AccountEvent::NewMessages { .. }), "non-snapshot events keep their position");
    let mailboxes: Vec<&MailboxId> = collapsed
        .iter()
        .filter_map(|event| match event {
            AccountEvent::MessagesUpdated { mailbox, .. } => Some(mailbox),
            _ => None,
        })
        .collect();
    assert_eq!(mailboxes, vec![&b, &a], "last copy per mailbox, in original order");
}

#[test]
fn collapse_account_events_keeps_everything_when_nothing_is_superseded() {
    let a = MailboxId("a:INBOX".into());
    let collapsed = collapse_account_events(vec![
        AccountEvent::MessagesUpdated {
            mailbox: a.clone(),
            messages: Vec::new(),
        },
        AccountEvent::MoveFailed {
            mailbox: a.clone(),
            uids: vec![Uid(7)],
            role: MailboxRole::Custom,
            message: "nope".into(),
        },
        AccountEvent::StoreFlagsFailed {
            mailbox: a.clone(),
            uids: vec![Uid(7)],
            message: "nope".into(),
        },
    ]);
    assert_eq!(collapsed.len(), 3, "no repeated snapshot key, nothing is dropped");
    assert!(matches!(&collapsed[1], AccountEvent::MoveFailed { .. }));
    assert!(matches!(&collapsed[2], AccountEvent::StoreFlagsFailed { .. }));
}

#[test]
fn collapse_calendar_events_keeps_the_last_occurrences_per_month() {
    let m1 = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let m2 = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    let occ = |month: chrono::NaiveDate| CalendarSessionEvent::OccurrencesUpdated { month, occurrences: Vec::new() };
    let collapsed = collapse_calendar_events(vec![CalendarSessionEvent::CalendarsUpdated(Vec::new()), occ(m1), occ(m2), occ(m1)]);
    assert_eq!(collapsed.len(), 3, "the earlier August copy is superseded");
    assert!(matches!(&collapsed[0], CalendarSessionEvent::CalendarsUpdated(_)));
    let months: Vec<&chrono::NaiveDate> = collapsed
        .iter()
        .filter_map(|event| match event {
            CalendarSessionEvent::OccurrencesUpdated { month, .. } => Some(month),
            _ => None,
        })
        .collect();
    assert_eq!(months, vec![&m2, &m1], "one copy per month, in original order");
}

#[test]
fn collapse_calendar_events_keeps_one_calendars_and_tasks_copy() {
    let collapsed = collapse_calendar_events(vec![
        CalendarSessionEvent::CalendarsUpdated(Vec::new()),
        CalendarSessionEvent::TasksUpdated(Vec::new()),
        CalendarSessionEvent::CalendarsUpdated(Vec::new()),
    ]);
    assert_eq!(collapsed.len(), 2);
    assert!(matches!(&collapsed[0], CalendarSessionEvent::TasksUpdated(_)));
    assert!(matches!(&collapsed[1], CalendarSessionEvent::CalendarsUpdated(_)));
}

#[test]
fn collapse_google_tasks_events_keeps_one_list_and_task_snapshot() {
    let collapsed = collapse_google_tasks_events(vec![
        GoogleTasksEvent::ListsUpdated(Vec::new()),
        GoogleTasksEvent::TasksUpdated(Vec::new()),
        GoogleTasksEvent::ListsUpdated(Vec::new()),
    ]);
    assert_eq!(collapsed.len(), 2);
    assert!(matches!(&collapsed[0], GoogleTasksEvent::TasksUpdated(_)));
    assert!(matches!(&collapsed[1], GoogleTasksEvent::ListsUpdated(_)));
}

#[test]
fn collapse_graph_todo_events_keeps_one_list_and_task_snapshot() {
    let collapsed = collapse_graph_todo_events(vec![
        lookout_graph::TodoEvent::ListsUpdated(Vec::new()),
        lookout_graph::TodoEvent::TasksUpdated(Vec::new()),
        lookout_graph::TodoEvent::ListsUpdated(Vec::new()),
    ]);
    assert_eq!(collapsed.len(), 2);
    assert!(matches!(&collapsed[0], lookout_graph::TodoEvent::TasksUpdated(_)));
    assert!(matches!(&collapsed[1], lookout_graph::TodoEvent::ListsUpdated(_)));
}

#[test]
fn collapse_subscription_events_keeps_the_last_update_per_month() {
    let m1 = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
    let m2 = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
    let feeds = |month: chrono::NaiveDate| SubscriptionSessionEvent::SubscriptionsUpdated { month, feeds: Vec::new() };
    let collapsed = collapse_subscription_events(vec![feeds(m1), feeds(m2), feeds(m1)]);
    assert_eq!(collapsed.len(), 2, "the earlier August update is superseded");
    let months: Vec<chrono::NaiveDate> = collapsed
        .iter()
        .map(|event| match event {
            SubscriptionSessionEvent::SubscriptionsUpdated { month, .. } => *month,
        })
        .collect();
    assert_eq!(months, vec![m2, m1]);
}

/// The message context menu's right-click rule (item 1.17): a row inside
/// the selection acts on the whole selection, any other row is selected
/// alone first, and a conversation header acts on its messages with the
/// newest one for the single-message verbs.
#[test]
fn message_menu_targets_follow_the_right_click_rule() {
    if !crate::gtk_test::gtk_ready() {
        return;
    }
    let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    let three = summary(Uid(3), "acc:INBOX", 2026, 8, 1, 11);
    let message_list = MessageListModel::build();
    message_list.set_threaded(false);
    message_list.repopulate(vec![one.clone(), two.clone(), three.clone()], SortKey::Date, true);
    let position_of = |uid: Uid| {
        (0..message_list.selection.n_items())
            .find(|&p| message_list.summary_at_position(p).is_some_and(|s| s.uid == uid))
            .expect("listed")
    };
    let uids = |summaries: &[EmailSummary]| summaries.iter().map(|s| s.uid.0).collect::<HashSet<_>>();

    message_list.selection.select_item(position_of(Uid(1)), true);
    message_list.selection.select_item(position_of(Uid(2)), false);
    let (targets, primary) = message_menu_targets(
        &message_list,
        MessageMenuTarget::Row {
            summary: Box::new(two.clone()),
            position: Some(position_of(Uid(2))),
        },
    );
    assert_eq!(uids(&targets), HashSet::from([1, 2]), "a row inside the selection acts on all of it");
    assert!(primary.is_none(), "no single message for Reply among several");
    assert_eq!(uids(&message_list.selected_summaries()), HashSet::from([1, 2]), "the selection is kept");

    let (targets, primary) = message_menu_targets(
        &message_list,
        MessageMenuTarget::Row {
            summary: Box::new(three.clone()),
            position: Some(position_of(Uid(3))),
        },
    );
    assert_eq!(uids(&targets), HashSet::from([3]));
    assert_eq!(primary.map(|s| s.uid), Some(Uid(3)));
    assert_eq!(uids(&message_list.selected_summaries()), HashSet::from([3]), "any other row is selected alone");

    // `summary()` gives every message the same thread key, so threading
    // makes one conversation of the three.
    message_list.set_threaded(true);
    let thread: ThreadId = (MailboxId("acc:INBOX".into()), lookout_core::ThreadKey("t".into()));
    let (targets, primary) = message_menu_targets(&message_list, MessageMenuTarget::Thread(thread));
    assert_eq!(uids(&targets), HashSet::from([1, 2, 3]), "a conversation header acts on every message in it");
    assert_eq!(primary.map(|s| s.uid), Some(Uid(3)), "its newest message answers Reply");

    // A selected header (item 1.7): Shift+F10 acts like a right-click on it.
    let is_header = |position: u32| {
        let row = message_list.selection.item(position).and_downcast::<gtk::TreeListRow>();
        let Some(boxed) = row.and_then(|r| r.item()).and_downcast::<glib::BoxedAnyObject>() else {
            return false;
        };
        let is_thread = matches!(&*boxed.borrow::<MessageItem>(), MessageItem::Thread(_));
        is_thread
    };
    let header = (0..message_list.selection.n_items()).find(|&p| is_header(p)).expect("a conversation header");
    message_list.selection.select_item(header, true);
    let (targets, primary) = message_menu_targets(&message_list, MessageMenuTarget::Selection);
    assert_eq!(uids(&targets), HashSet::from([1, 2, 3]));
    assert_eq!(primary.map(|s| s.uid), Some(Uid(3)));

    // A conversation card's More actions acts on its message alone,
    // leaving the selection be.
    let (targets, primary) = message_menu_targets(&message_list, MessageMenuTarget::Message(Box::new(one.clone())));
    assert_eq!(uids(&targets), HashSet::from([1]));
    assert_eq!(primary.map(|s| s.uid), Some(Uid(1)));
    assert!(matches!(message_list.selection_kind(), SelectionKind::Thread(..)), "the header stays selected");
}

/// Categorize's checks over several messages: checked when all carry the
/// category, mixed when some do.
#[test]
fn tag_check_state_is_all_some_or_none() {
    let keyword = lookout_core::tag_keyword("work");
    let mut tagged = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    tagged.keywords.insert(keyword.clone());
    let plain = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
    assert_eq!(tag_check_state(&[tagged.clone(), tagged.clone()], &keyword), (true, false));
    assert_eq!(tag_check_state(&[tagged, plain.clone()], &keyword), (false, true));
    assert_eq!(tag_check_state(&[plain], &keyword), (false, false));
    assert_eq!(tag_check_state(&[], &keyword), (false, false));
}

/// Categorize over a mixed set only stores the keyword on the messages
/// that lack it, and Undo takes it off exactly those.
#[test]
fn categorize_touches_only_the_messages_it_changes() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;
    let keyword = lookout_core::tag_keyword("work");
    let mut tagged = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
    tagged.keywords.insert(keyword.clone());
    let plain = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

    send_keyword_store(&state, &[tagged, plain], &keyword, true);
    match cmd_rx.try_recv() {
        Ok(AccountCommand::StoreKeywordsMany { uids, add, remove, .. }) => {
            assert_eq!(uids, vec![Uid(2)]);
            assert_eq!(add, vec![keyword.clone()]);
            assert!(remove.is_empty());
        }
        other => panic!("expected StoreKeywordsMany, got {other:?}"),
    }
    assert!(undo_last(&state));
    match cmd_rx.try_recv() {
        Ok(AccountCommand::StoreKeywordsMany { uids, add, remove, .. }) => {
            assert_eq!(uids, vec![Uid(2)]);
            assert!(add.is_empty());
            assert_eq!(remove, vec![keyword]);
        }
        other => panic!("expected the undo StoreKeywordsMany, got {other:?}"),
    }
}

/// Copy ▸ sends one `CopyMessagesTo` per source mailbox (skipping
/// messages already in the target), makes the target a recent folder,
/// and Undo sends `RevertCopy` for exactly what was copied.
#[test]
fn copy_to_folder_records_recent_folder_and_undoes_by_revert_copy() {
    let acc = AccountId("acc".into());
    let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 1), test_mailbox(&acc, "Projects", 0)])]);
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;
    let projects = MailboxId("acc:Projects".into());
    let inbox_message = summary(Uid(4), "acc:INBOX", 2026, 8, 1, 9);
    let already_there = summary(Uid(9), "acc:Projects", 2026, 8, 1, 10);

    copy_messages_to_folder(&state, &[inbox_message, already_there], &projects);
    match cmd_rx.try_recv() {
        Ok(AccountCommand::CopyMessagesTo { mailbox, uids, target }) => {
            assert_eq!(mailbox, MailboxId("acc:INBOX".into()));
            assert_eq!(uids, vec![Uid(4)]);
            assert_eq!(target, projects);
        }
        other => panic!("expected CopyMessagesTo, got {other:?}"),
    }
    assert!(cmd_rx.try_recv().is_err(), "the message already in Projects isn't copied");
    assert_eq!(state.borrow().recent_folders[&acc], vec![projects.clone()]);

    assert!(undo_last(&state));
    match cmd_rx.try_recv() {
        Ok(AccountCommand::RevertCopy { copied_from, copied_to, uids }) => {
            assert_eq!(copied_from, MailboxId("acc:INBOX".into()));
            assert_eq!(copied_to, projects);
            assert_eq!(uids, vec![Uid(4)]);
        }
        other => panic!("expected RevertCopy, got {other:?}"),
    }
}
