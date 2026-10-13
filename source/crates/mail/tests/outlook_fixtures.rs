//! Outlook artefact fixtures (OUTLOOK_PARITY.md item 0.8): every text fixture
//! under `test-fixtures/outlook/` parses through the same readers the app uses,
//! and its signature fields come back as expected.

use std::path::PathBuf;

use lookout_core::{header_value, importance_from_headers, parse_imip_method, sensitivity_from_headers, EmailBody, ImipMethod, Importance, MessageSensitivity, Uid};
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
    // COUNTER isn't modelled yet (CAL-17), so pin the raw property it must keep.
    assert!(ics("counter.eml").contains("METHOD:COUNTER"));
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
