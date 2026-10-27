//! The local Focused Inbox classifier (OUTLOOK_PARITY.md §4.1, M-03 /
//! item 2.12).
//!
//! Microsoft 365 carries Outlook's own `inferenceClassification` through
//! Graph, so this module is only used for every other account: the mail
//! session feeds it a message's sender, its `List-Id`, any explicit
//! per-sender override, and whether the sender is already known (in the
//! user's contacts or in Sent). The rules are deliberately small and
//! predictable, matching the agreed priority:
//!
//! 1. an explicit sender override wins over everything;
//! 2. a known sender is Focused;
//! 3. mail carrying a `List-Id` is bulk and goes to Other;
//! 4. everything else, including mail with no sender, stays Focused.
//!
//! No I/O, no GTK, and no Microsoft-specific wire strings live here - the
//! caller does the cache reads and supplies the four plain inputs.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use super::InferenceClassification;

/// Normalises an email address for comparison: surrounding whitespace is
/// trimmed and the rest is lowercased. The same form is used for the known
/// sender set, the Sent-recipient set and each message's sender, so matching
/// is case-insensitive no matter which stored spelling wins.
pub fn normalize_address(address: &str) -> String {
    address.trim().to_lowercase()
}

/// Classifies one message for the local Focused Inbox.
///
/// `sender` is the message's first `From` address (if any), `list_id` its
/// RFC 2919 `List-Id` header, `override_class` an explicit per-sender choice
/// from the cache, and `known` whether the sender appears in the user's
/// contacts or Sent mail. Address comparisons are normalised, so callers may
/// pass either the stored or a lowercased spelling.
pub fn classify(sender: Option<&str>, list_id: Option<&str>, override_class: Option<InferenceClassification>, known: bool) -> InferenceClassification {
    if let Some(classification) = override_class {
        return classification;
    }
    let sender = sender.map(normalize_address).filter(|address| !address.is_empty());
    if sender.is_some() && known {
        return InferenceClassification::Focused;
    }
    if list_id.is_some_and(|list_id| !list_id.trim().is_empty()) {
        return InferenceClassification::Other;
    }
    InferenceClassification::Focused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_wins_over_known_and_list_mail() {
        assert_eq!(
            classify(Some("ada@example.com"), Some("announce.example.com"), Some(InferenceClassification::Other), true),
            InferenceClassification::Other
        );
        assert_eq!(
            classify(Some("ada@example.com"), Some("announce.example.com"), Some(InferenceClassification::Focused), false),
            InferenceClassification::Focused
        );
    }

    #[test]
    fn a_known_sender_beats_a_list_id() {
        assert_eq!(
            classify(Some("ada@example.com"), Some("announce.example.com"), None, true),
            InferenceClassification::Focused
        );
    }

    #[test]
    fn a_list_id_alone_goes_to_other() {
        assert_eq!(
            classify(Some("news@example.com"), Some("<announce.example.com>"), None, false),
            InferenceClassification::Other
        );
    }

    #[test]
    fn an_unknown_sender_stays_focused() {
        assert_eq!(classify(Some("ada@example.com"), None, None, false), InferenceClassification::Focused);
        assert_eq!(classify(Some("ada@example.com"), Some("   "), None, false), InferenceClassification::Focused);
    }

    #[test]
    fn sender_matching_is_case_insensitive() {
        let stored = normalize_address("  Ada@Example.COM ");
        assert_eq!(stored, "ada@example.com");
        let sender = "  ADA@example.com  ";
        assert_eq!(normalize_address(sender), stored, "the same person's spellings normalise alike");
        assert_eq!(classify(Some(sender), Some("list.example.com"), None, true), InferenceClassification::Focused);
        assert_eq!(classify(Some("ada@example.com"), Some("list.example.com"), None, false), InferenceClassification::Other);
    }

    #[test]
    fn a_missing_sender_stays_focused() {
        assert_eq!(classify(None, None, None, true), InferenceClassification::Focused);
        assert_eq!(classify(Some("   "), None, None, true), InferenceClassification::Focused);
    }
}
