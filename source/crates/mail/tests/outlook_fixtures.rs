//! Outlook artefact fixtures (OUTLOOK_PARITY.md item 0.8): every text fixture
//! under `test-fixtures/outlook/` parses through the same readers the app uses,
//! and its signature fields come back as expected.

use std::path::PathBuf;

use lookout_core::{
    header_value, importance_from_headers, parse_imip_method, sensitivity_from_headers, DeliveryReportKind, EmailBody, ImipMethod, Importance, MessageSensitivity, ReportAction,
    Uid,
};
use lookout_mail::body::parse_body;

fn outlook_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/outlook"))
}

fn parse(name: &str) -> EmailBody {
    let path = outlook_dir().join(name);
    let raw = std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    parse_body(Uid(0), &raw).unwrap_or_else(|| panic!("{name} must parse as a message"))
}

fn ics(name: &str) -> String {
    parse(name).calendar_ics.unwrap_or_else(|| panic!("{name} must carry a text/calendar payload"))
}

#[test]
fn every_checked_in_outlook_fixture_parses() {
    let mut count = 0;
    for entry in std::fs::read_dir(outlook_dir()).expect("the outlook fixtures directory exists") {
        let path = entry.expect("readable directory entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("eml") {
            continue;
        }
        let raw = std::fs::read(&path).expect("readable fixture");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(parse_body(Uid(0), &raw).is_some(), "{name} must parse as a message");
        count += 1;
    }
    assert!(count >= 8, "expected the full Outlook corpus, found {count} .eml fixtures");
}

#[test]
fn tnef_fixture_exposes_its_inner_attachments() {
    let path = outlook_dir().join("tnef-message.eml");
    let raw = std::fs::read(&path).expect("the synthetic winmail.dat message is checked in");
    let body = parse_body(Uid(0), &raw).expect("parses as a message");

    // The raw `winmail.dat` part is replaced by the payload's attachments,
    // numbered into the synthetic `tnef:<source>:<index>` scheme the session's
    // FetchAttachment answers.
    assert!(body.parts.iter().all(|part| !part.is_tnef()));
    let report = body.parts.iter().find(|part| part.part_number == "tnef:2:1").expect("text attachment decoded");
    assert_eq!(report.filename.as_deref(), Some("report.txt"));
    assert!(report.is_attachment);
    let logo = body.parts.iter().find(|part| part.part_number == "tnef:2:2").expect("inline image decoded");
    assert_eq!(logo.cid.as_deref(), Some("logo123@lookout.test"));
    assert!(!logo.is_attachment);

    assert_eq!(body.html_body.as_deref(), Some("<html><body><p>Hello <b>world</b></p></body></html>"));
    assert!(body.calendar_ics.as_deref().is_some_and(|ics| ics.contains("METHOD:REQUEST")));
}

#[test]
fn invitations_carry_their_itip_method() {
    assert_eq!(parse_imip_method(&ics("invite.eml")), ImipMethod::Request);
    assert_eq!(parse_imip_method(&ics("invite-update.eml")), ImipMethod::Request);
    assert_eq!(parse_imip_method(&ics("cancel.eml")), ImipMethod::Cancel);
    assert_eq!(parse_imip_method(&ics("teams-invite.eml")), ImipMethod::Request);
    // CAL-17: an attendee's new-time proposal is a COUNTER.
    assert_eq!(parse_imip_method(&ics("counter.eml")), ImipMethod::Counter);

    // CAL-17: Outlook's "disallow time proposals" flag is read off the invite.
    assert!(lookout_core::disallow_counter(&ics("disallow-counter.eml")));
    assert!(!lookout_core::disallow_counter(&ics("invite.eml")));
}

#[test]
fn invitation_fixtures_carry_microsoft_extensions() {
    let invite = ics("invite.eml");
    assert!(invite.contains("PRODID:Microsoft Exchange Server 2019"));
    assert!(invite.contains("X-MICROSOFT-CDO-BUSYSTATUS:BUSY"));
    assert!(invite.contains("TZID=W. Europe Standard Time"));

    let update = ics("invite-update.eml");
    assert!(update.contains("SEQUENCE:1"));
    assert!(update.contains("X-MICROSOFT-CDO-APPT-SEQUENCE:1"));

    assert!(ics("cancel.eml").contains("STATUS:CANCELLED"));

    let teams = ics("teams-invite.eml");
    assert!(teams.contains("X-MICROSOFT-SKYPETEAMSMEETINGURL:https://teams.microsoft.com/"));
}

#[test]
fn authentication_results_drive_the_unverified_sender_verdict() {
    let body = parse("auth-results.eml");
    let results = body.auth_results.as_ref().expect("Authentication-Results parsed");
    assert_eq!(results.spf, Some(lookout_core::SpfResult::SoftFail));
    assert_eq!(results.dkim, Some(lookout_core::DkimResult::Fail));
    assert_eq!(results.dmarc, Some(lookout_core::DmarcResult::Fail));
    assert!(results.any_failure());
    assert!(!results.any_pass());

    // The display name hides a different address (the phishing shape).
    let from = lookout_core::EmailAddress {
        name: Some("support@paypal.com".to_string()),
        address: "attacker@evil.example".to_string(),
    };
    assert_eq!(lookout_core::display_name_address_mismatch(&from).as_deref(), Some("support@paypal.com"));
}

#[test]
fn importance_and_sensitivity_headers_round_trip() {
    let body = parse("importance-high.eml");
    assert_eq!(importance_from_headers(&body.headers), Some(Importance::High));
    assert_eq!(header_value(&body.headers, "List-Id"), Some("Contoso Notifications <notifications.contoso.com>"));
    assert_eq!(sensitivity_from_headers(&body.headers), Some(MessageSensitivity::Private));

    let confidential = parse("sensitivity-private.eml");
    assert_eq!(sensitivity_from_headers(&confidential.headers), Some(MessageSensitivity::CompanyConfidential));
}

/// The three RFC 3464 delivery status notification shapes (M-27 / C-07): the
/// machine-readable block parses into a report with the right verdict, the
/// report's prose stays the body, and none of its parts leak into the
/// attachment strip.
#[test]
fn dsn_fixtures_expose_their_delivery_reports() {
    let failed = parse("dsn-failed.eml");
    let report = failed.dsn.clone().expect("the failed DSN parses");
    assert_eq!(report.kind, DeliveryReportKind::Failed);
    assert_eq!(report.reporting_mta.as_deref(), Some("mx.contoso.com"));
    assert_eq!(report.original_subject.as_deref(), Some("Quarterly report"));
    assert_eq!(report.original_message_id.as_deref(), Some("quarterly-20260112@example.com"));
    assert_eq!(report.recipients.len(), 1);
    let recipient = &report.recipients[0];
    assert_eq!(recipient.final_recipient, "nobody@contoso.com");
    assert_eq!(recipient.action, ReportAction::Failed);
    assert_eq!(recipient.status.as_deref(), Some("5.1.1"));
    assert!(recipient.diagnostic_code.as_deref().is_some_and(|code| code.contains("User unknown")));
    assert!(failed.text_body.as_deref().is_some_and(|text| text.contains("could not")));
    assert!(failed.parts.iter().all(|part| !part.is_delivery_status() && !part.is_rfc822_headers()));

    let delivered = parse("dsn-delivered.eml").dsn.expect("the delivered DSN parses");
    assert_eq!(delivered.kind, DeliveryReportKind::Delivered);
    assert_eq!(delivered.recipients[0].action, ReportAction::Delivered);
    assert_eq!(delivered.recipients[0].status.as_deref(), Some("2.0.0"));

    let delayed = parse("dsn-delayed.eml").dsn.expect("the delayed DSN parses");
    assert_eq!(delayed.kind, DeliveryReportKind::Delayed);
    assert_eq!(delayed.recipients[0].action, ReportAction::Delayed);
    assert_eq!(delayed.recipients[0].status.as_deref(), Some("4.4.1"));

    // A read receipt is a report too, but never a delivery report.
    assert_eq!(parse("auth-results.eml").dsn, None);
}
