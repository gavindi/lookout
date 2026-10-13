//! Message importance and sensitivity, as Outlook writes and reads them
//! (OUTLOOK_PARITY.md §4.1).
//!
//! Importance travels under four different header names depending on the
//! client and era, so reading accepts all of them in a fixed precedence order
//! and writing emits all three Outlook names at once. Sensitivity is Outlook's
//! RFC 2156 `Sensitivity` header (a per-message concept) and is deliberately a
//! *different* type from the calendar's RFC 5545 `CLASS`
//! ([`crate::calendar::EventSensitivity`]) - the two share no values.
//!
//! Only parsing and emission live here; persistence on `EmailSummary` is a
//! later item (0.2). `Authentication-Results` parsing (6.1 / S-03, M-35) also
//! lives here: it is read off the wire and drives the "unverified sender"
//! banner alongside importance and sensitivity.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use crate::email::{header_value, AuthenticationResults, DkimResult, DmarcResult, SpfResult};

/// `Importance:` (RFC 2156) — the canonical Outlook name.
pub const IMPORTANCE_HEADER: &str = "Importance";
/// `X-Priority:` — the classic Outlook numeric form, `1 (Highest)` … `5 (Lowest)`.
pub const X_PRIORITY_HEADER: &str = "X-Priority";
/// `X-MSMail-Priority:` — the classic Outlook word form.
pub const X_MSMAIL_PRIORITY_HEADER: &str = "X-MSMail-Priority";
/// `Priority:` — RFC 2156's own spelling, `urgent` / `normal` / `non-urgent`.
pub const PRIORITY_HEADER: &str = "Priority";
/// `Sensitivity:` (RFC 2156).
pub const SENSITIVITY_HEADER: &str = "Sensitivity";
/// `Authentication-Results:` (RFC 8601) — added by the receiving boundary
/// (the server, or a scanner), not by the sender. Its `spf` / `dkim` / `dmarc`
/// method verdicts drive the reading pane's "unverified sender" banner.
pub const AUTHENTICATION_RESULTS_HEADER: &str = "Authentication-Results";

/// A message's importance (Graph `importance: low|normal|high`). The `Normal`
/// default matches Outlook: an unmarked message is normal importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
}

impl Importance {
    /// Parses Graph's lowercase `importance` value. Unknown values yield
    /// `None` so callers can fall back to a header-derived or default value.
    pub fn from_graph(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Importance::Low),
            "normal" => Some(Importance::Normal),
            "high" => Some(Importance::High),
            _ => None,
        }
    }

    /// Graph's `importance` value for writing.
    pub fn as_graph_str(self) -> &'static str {
        match self {
            Importance::Low => "low",
            Importance::Normal => "normal",
            Importance::High => "high",
        }
    }
}

/// A message's sensitivity (RFC 2156 `Sensitivity`). `Normal` is the default
/// and emits no header, matching Outlook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum MessageSensitivity {
    #[default]
    Normal,
    Personal,
    Private,
    CompanyConfidential,
}

impl MessageSensitivity {
    /// Parses the header value. Accepts both the hyphenated RFC 2156 form and
    /// the space-separated spelling seen in the wild.
    pub fn from_header_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(MessageSensitivity::Normal),
            "personal" => Some(MessageSensitivity::Personal),
            "private" => Some(MessageSensitivity::Private),
            "company-confidential" | "company confidential" => Some(MessageSensitivity::CompanyConfidential),
            _ => None,
        }
    }

    /// The header value to write, or `None` for `Normal` (which Outlook omits).
    pub fn as_header_value(self) -> Option<&'static str> {
        match self {
            MessageSensitivity::Normal => None,
            MessageSensitivity::Personal => Some("Personal"),
            MessageSensitivity::Private => Some("Private"),
            MessageSensitivity::CompanyConfidential => Some("Company-Confidential"),
        }
    }
}

/// Reads importance from a message's `(name, value)` header list, trying each
/// of the four names in precedence order: `Importance`, then `X-Priority`,
/// then `X-MSMail-Priority`, then RFC 2156 `Priority`. Returns `None` when no
/// header is present or none of them parses.
pub fn importance_from_headers(headers: &[(String, String)]) -> Option<Importance> {
    if let Some(importance) = header_value(headers, IMPORTANCE_HEADER).and_then(importance_from_word) {
        return Some(importance);
    }
    if let Some(importance) = header_value(headers, X_PRIORITY_HEADER).and_then(importance_from_x_priority) {
        return Some(importance);
    }
    if let Some(importance) = header_value(headers, X_MSMAIL_PRIORITY_HEADER).and_then(importance_from_word) {
        return Some(importance);
    }
    header_value(headers, PRIORITY_HEADER).and_then(importance_from_rfc2156)
}

/// Reads sensitivity from a message's `(name, value)` header list. Returns
/// `None` when the header is absent or unrecognised.
pub fn sensitivity_from_headers(headers: &[(String, String)]) -> Option<MessageSensitivity> {
    header_value(headers, SENSITIVITY_HEADER).and_then(MessageSensitivity::from_header_value)
}

/// The three Outlook headers to write for `importance`: `Importance`,
/// `X-Priority` and `X-MSMail-Priority`, so every client reads the value.
pub fn importance_headers(importance: Importance) -> [(&'static str, String); 3] {
    let word = match importance {
        Importance::Low => "Low",
        Importance::Normal => "Normal",
        Importance::High => "High",
    };
    let x_priority = match importance {
        Importance::Low => "5 (Lowest)",
        Importance::Normal => "3 (Normal)",
        Importance::High => "1 (Highest)",
    };
    [
        (IMPORTANCE_HEADER, word.to_string()),
        (X_PRIORITY_HEADER, x_priority.to_string()),
        (X_MSMAIL_PRIORITY_HEADER, word.to_string()),
    ]
}

/// The single header to write for `sensitivity`, or `None` for the default
/// `Normal` (which Outlook leaves off the wire).
pub fn sensitivity_header(sensitivity: MessageSensitivity) -> Option<(&'static str, &'static str)> {
    sensitivity.as_header_value().map(|value| (SENSITIVITY_HEADER, value))
}

/// Reads a message's `Authentication-Results` header(s) (RFC 8601) into the
/// `spf` / `dkim` / `dmarc` verdicts each method reported. The header is a
/// `authserv-id` followed by semicolon-separated `method=result` clauses, e.g.
/// `mx.example.com; spf=pass smtp.mailfrom=a.com; dkim=softfail; dmarc=fail`.
/// Clauses may carry trailing `property=value` detail (`header.d=…`,
/// `smtp.mailfrom=…`, `reason=…`) which is ignored; the `auth` overall result
/// has no field here and is skipped.
///
/// Returns `None` when no header is present or none of `spf` / `dkim` /
/// `dmarc` carried a recognised verdict (so a message with no usable
/// authentication information never raises a banner). When several headers or
/// clauses name the same method, the first occurrence in wire order wins —
/// the closest (topmost) authentication boundary is the most authoritative.
pub fn auth_results_from_headers(headers: &[(String, String)]) -> Option<AuthenticationResults> {
    let mut out = AuthenticationResults::default();
    let mut found = false;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case(AUTHENTICATION_RESULTS_HEADER) {
            continue;
        }
        for clause in value.split(|c: char| c == ';' || c.is_whitespace()) {
            let Some((method, result)) = clause.split_once('=') else {
                continue;
            };
            let result = result.trim().trim_matches('"').to_ascii_lowercase();
            let applied = match method.trim().to_ascii_lowercase().as_str() {
                "spf" => set_once(&mut out.spf, spf_result(&result)),
                "dkim" => set_once(&mut out.dkim, dkim_result(&result)),
                "dmarc" => set_once(&mut out.dmarc, dmarc_result(&result)),
                _ => false,
            };
            found |= applied;
        }
    }
    found.then_some(out)
}

/// Fills `slot` from `value` only while it is still empty; reports whether a
/// value was written, so the caller can tell a recognised method from an
/// unknown one.
fn set_once<T>(slot: &mut Option<T>, value: Option<T>) -> bool {
    if slot.is_none() {
        if let Some(value) = value {
            *slot = Some(value);
            return true;
        }
    }
    false
}

fn spf_result(value: &str) -> Option<SpfResult> {
    match value {
        "pass" => Some(SpfResult::Pass),
        "fail" => Some(SpfResult::Fail),
        "softfail" => Some(SpfResult::SoftFail),
        "neutral" => Some(SpfResult::Neutral),
        "none" => Some(SpfResult::None),
        "temperror" => Some(SpfResult::TempError),
        "permerror" => Some(SpfResult::PermError),
        _ => None,
    }
}

fn dkim_result(value: &str) -> Option<DkimResult> {
    match value {
        "pass" => Some(DkimResult::Pass),
        "fail" => Some(DkimResult::Fail),
        "policy" => Some(DkimResult::Policy),
        "neutral" => Some(DkimResult::Neutral),
        "none" => Some(DkimResult::None),
        "temperror" => Some(DkimResult::TempError),
        "permerror" => Some(DkimResult::PermError),
        _ => None,
    }
}

fn dmarc_result(value: &str) -> Option<DmarcResult> {
    match value {
        "pass" => Some(DmarcResult::Pass),
        "fail" => Some(DmarcResult::Fail),
        "none" => Some(DmarcResult::None),
        "temperror" => Some(DmarcResult::TempError),
        "permerror" => Some(DmarcResult::PermError),
        _ => None,
    }
}

/// `High`/`Normal`/`Low` (also tolerates `medium`), case-insensitively.
fn importance_from_word(value: &str) -> Option<Importance> {
    match value.trim().to_ascii_lowercase().as_str() {
        "high" => Some(Importance::High),
        "normal" | "medium" => Some(Importance::Normal),
        "low" => Some(Importance::Low),
        _ => None,
    }
}

/// The leading integer of an `X-Priority` value (`"1 (Highest)"` → `1`),
/// mapped to the RFC 2156 scale: 1–2 high, 3 normal, 4–5 low.
fn importance_from_x_priority(value: &str) -> Option<Importance> {
    let digits: String = value.trim().chars().take_while(char::is_ascii_digit).collect();
    match digits.as_str() {
        "1" | "2" => Some(Importance::High),
        "3" => Some(Importance::Normal),
        "4" | "5" => Some(Importance::Low),
        _ => None,
    }
}

/// RFC 2156 `Priority`: `urgent` / `normal` / `non-urgent`.
fn importance_from_rfc2156(value: &str) -> Option<Importance> {
    match value.trim().to_ascii_lowercase().as_str() {
        "urgent" => Some(Importance::High),
        "normal" => Some(Importance::Normal),
        "non-urgent" => Some(Importance::Low),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn importance_reads_each_supported_header() {
        assert_eq!(importance_from_headers(&headers(&[("Importance", "High")])), Some(Importance::High));
        assert_eq!(importance_from_headers(&headers(&[("importance", "low")])), Some(Importance::Low));
        assert_eq!(importance_from_headers(&headers(&[("X-Priority", "1 (Highest)")])), Some(Importance::High));
        assert_eq!(importance_from_headers(&headers(&[("X-Priority", "4")])), Some(Importance::Low));
        assert_eq!(importance_from_headers(&headers(&[("X-MSMail-Priority", "High")])), Some(Importance::High));
        assert_eq!(importance_from_headers(&headers(&[("Priority", "urgent")])), Some(Importance::High));
        assert_eq!(importance_from_headers(&headers(&[("Priority", "non-urgent")])), Some(Importance::Low));
    }

    #[test]
    fn importance_precedence_is_importance_then_x_priority_then_msmail_then_rfc2156() {
        let all = headers(&[
            ("Priority", "non-urgent"),
            ("X-MSMail-Priority", "Low"),
            ("X-Priority", "1 (Highest)"),
            ("Importance", "Normal"),
        ]);
        assert_eq!(importance_from_headers(&all), Some(Importance::Normal));
        let no_importance = headers(&[("Priority", "non-urgent"), ("X-MSMail-Priority", "Low"), ("X-Priority", "1 (Highest)")]);
        assert_eq!(importance_from_headers(&no_importance), Some(Importance::High));
    }

    #[test]
    fn importance_absent_or_unknown_is_none() {
        assert_eq!(importance_from_headers(&[]), None);
        assert_eq!(importance_from_headers(&headers(&[("Importance", "banana")])), None);
        assert_eq!(importance_from_headers(&headers(&[("X-Priority", "")])), None);
        assert_eq!(importance_from_headers(&headers(&[("X-Priority", "9")])), None);
    }

    #[test]
    fn importance_headers_round_trip_through_the_reader() {
        for importance in [Importance::Low, Importance::Normal, Importance::High] {
            let written = importance_headers(importance);
            assert_eq!(written[1].0, X_PRIORITY_HEADER);
            assert_eq!(importance_from_headers(&written.map(|(k, v)| (k.to_string(), v))), Some(importance));
        }
    }

    #[test]
    fn importance_graph_strings_round_trip() {
        for importance in [Importance::Low, Importance::Normal, Importance::High] {
            assert_eq!(Importance::from_graph(importance.as_graph_str()), Some(importance));
        }
        assert_eq!(Importance::from_graph("bogus"), None);
    }

    #[test]
    fn sensitivity_reads_and_round_trips() {
        assert_eq!(sensitivity_from_headers(&headers(&[("Sensitivity", "Personal")])), Some(MessageSensitivity::Personal));
        assert_eq!(
            sensitivity_from_headers(&headers(&[("sensitivity", "Company-Confidential")])),
            Some(MessageSensitivity::CompanyConfidential)
        );
        assert_eq!(
            sensitivity_from_headers(&headers(&[("Sensitivity", "company confidential")])),
            Some(MessageSensitivity::CompanyConfidential)
        );
        assert_eq!(sensitivity_from_headers(&[]), None);
        assert_eq!(sensitivity_from_headers(&headers(&[("Sensitivity", "Secret")])), None);
    }

    #[test]
    fn normal_sensitivity_emits_no_header() {
        assert_eq!(sensitivity_header(MessageSensitivity::Normal), None);
        assert_eq!(sensitivity_header(MessageSensitivity::Private), Some((SENSITIVITY_HEADER, "Private")));
    }

    #[test]
    fn auth_results_parses_all_three_methods() {
        let h = headers(&[(
            "Authentication-Results",
            "mx.example.com; spf=pass smtp.mailfrom=a.com; dkim=fail header.d=a.com; dmarc=pass header.from=a.com",
        )]);
        let parsed = auth_results_from_headers(&h).expect("header present");
        assert_eq!(parsed.spf, Some(SpfResult::Pass));
        assert_eq!(parsed.dkim, Some(DkimResult::Fail));
        assert_eq!(parsed.dmarc, Some(DmarcResult::Pass));
        assert!(parsed.any_pass());
        assert!(parsed.any_failure());
    }

    #[test]
    fn auth_results_maps_softfail_and_errors() {
        let soft = headers(&[("authentication-results", "h; spf=softfail")]);
        assert_eq!(auth_results_from_headers(&soft).unwrap().spf, Some(SpfResult::SoftFail));
        let err = headers(&[("Authentication-Results", "h; dkim=temperror; dmarc=permerror")]);
        let parsed = auth_results_from_headers(&err).unwrap();
        assert_eq!(parsed.dkim, Some(DkimResult::TempError));
        assert_eq!(parsed.dmarc, Some(DmarcResult::PermError));
        assert!(!parsed.any_pass());
        assert!(!parsed.any_failure());
    }

    #[test]
    fn auth_results_first_clause_wins_and_unknown_is_ignored() {
        let h = headers(&[("Authentication-Results", "h; spf=none; spf=pass; auth=pass; dmarc=banana")]);
        let parsed = auth_results_from_headers(&h).unwrap();
        assert_eq!(parsed.spf, Some(SpfResult::None));
        // `auth` has no field and `dmarc=banana` is unknown, so dmarc stays None.
        assert_eq!(parsed.dmarc, None);
    }

    #[test]
    fn auth_results_absent_or_unrecognised_is_none() {
        assert_eq!(auth_results_from_headers(&[]), None);
        assert_eq!(auth_results_from_headers(&headers(&[("Authentication-Results", "h; auth=pass")])), None);
        assert_eq!(auth_results_from_headers(&headers(&[("Subject", "spf=pass")])), None);
    }
}
