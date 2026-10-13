//! Junk-mail keywords (OUTLOOK_PARITY.md §4.2, M-17).
//!
//! RFC 5788 registers `$Junk` and `$NotJunk` as standard IMAP keywords. Outlook
//! itself keeps junk state in MAPI/Graph, but over IMAP these atoms are the
//! cross-client convention for teaching server-side spam filters: setting
//! `$Junk` when a message is reported as junk and `$NotJunk` when the user says
//! it isn't. The two are mutually exclusive, so the change always adds one and
//! removes the other.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
/// RFC 5788 keyword marking a message the user has reported as junk.
pub const JUNK_KEYWORD: &str = "$Junk";
/// RFC 5788 keyword marking a message the user says is *not* junk.
pub const NOT_JUNK_KEYWORD: &str = "$NotJunk";

/// The keyword recording a message's junk state: `$Junk` for `junk`, else
/// `$NotJunk`.
pub fn junk_keyword(junk: bool) -> &'static str {
    if junk {
        JUNK_KEYWORD
    } else {
        NOT_JUNK_KEYWORD
    }
}

/// The `(add, remove)` keyword change for marking a message junk (`junk ==
/// true`) or not junk: the two atoms stay mutually exclusive in one STORE.
pub fn junk_keyword_change(junk: bool) -> (&'static str, &'static str) {
    if junk {
        (JUNK_KEYWORD, NOT_JUNK_KEYWORD)
    } else {
        (NOT_JUNK_KEYWORD, JUNK_KEYWORD)
    }
}

/// Both junk keywords - what an undo removes to clear the state.
pub fn junk_keywords() -> [&'static str; 2] {
    [JUNK_KEYWORD, NOT_JUNK_KEYWORD]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_selects_and_clears_the_opposite_atom() {
        assert_eq!(junk_keyword(true), JUNK_KEYWORD);
        assert_eq!(junk_keyword(false), NOT_JUNK_KEYWORD);
        assert_eq!(junk_keyword_change(true), (JUNK_KEYWORD, NOT_JUNK_KEYWORD));
        assert_eq!(junk_keyword_change(false), (NOT_JUNK_KEYWORD, JUNK_KEYWORD));
        assert_eq!(junk_keywords(), [JUNK_KEYWORD, NOT_JUNK_KEYWORD]);
    }
}
