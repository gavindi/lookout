//! The mail search query language (OUTLOOK_PARITY.md §5.1 M-18, roadmap 2.2).
//!
//! Turns a user-typed query - free text plus Outlook-style operators - into
//! three derived forms:
//!
//! - a local SQLite FTS5 `MATCH` expression plus extra `json_extract`
//!   predicates ([`SearchQuery::fts_match`], [`SearchQuery::sql_predicates`]),
//!   consumed by `Cache::search_query`;
//! - an IMAP `UID SEARCH` criterion string ([`SearchQuery::to_imap_criteria`])
//!   for the server fallback (`session::search_mailbox`), so unsynced mail and
//!   never-fetched bodies are still covered;
//! - a summary evaluator ([`SearchQuery::matches_summary`]) that re-checks the
//!   predicates IMAP can't express (`has:attachment`, `category:`, and the
//!   JSON-only fields) against the fallback's fetched envelopes.
//!
//! Supported operators (case-insensitive): `from: to: cc: subject: body:
//! has:attachment is:unread is:read is:flagged is:pinned category: received:
//! size: folder:`. Anything else is free text. Values may be quoted to include
//! spaces (`subject:"q3 report"`). Free text keeps the neutralising behaviour
//! of [`crate::cache::sanitize_fts_query`]: each bare word is a quoted phrase
//! and FTS5 operators in user text can never change the query's shape.
//!
//! "Flagged" is the follow-up flag (`is:flagged`), distinct from `is:pinned`
//! (the `\Flagged` system flag) - see OUTLOOK_PARITY.md §4.2.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use lookout_core::{sanitize_tag_key, tag_keyword, EmailAddress, EmailSummary, FLAG_DONE_KEYWORD, FLAG_DUE_KEYWORD_PREFIX, FLAG_KEYWORD};

use crate::cache::sanitize_fts_query;

/// Where a search looks, from narrowest to widest (roadmap 2.3; the dropdown
/// in the title bar). `Account` and `All` are resolved by the app: within a
/// single account's cache both cover every folder, but only `All` asks every
/// account's cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchScope {
    /// Only the mailbox the user is viewing.
    Folder,
    /// The viewed mailbox plus its descendants in the folder tree.
    Subfolders,
    /// Every folder of the current account.
    Account,
    /// Every folder of every connected account.
    #[default]
    All,
}

impl SearchScope {
    /// The stable string stored in GSettings.
    pub fn as_str(self) -> &'static str {
        match self {
            SearchScope::Folder => "folder",
            SearchScope::Subfolders => "subfolders",
            SearchScope::Account => "account",
            SearchScope::All => "all",
        }
    }

    /// Parses a stored scope string, falling back to [`SearchScope::All`].
    pub fn from_setting(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "folder" => SearchScope::Folder,
            "subfolders" => SearchScope::Subfolders,
            "account" => SearchScope::Account,
            _ => SearchScope::All,
        }
    }

    /// The user-facing label, matching Outlook's scope wording (UX-11).
    pub fn label(self) -> &'static str {
        match self {
            SearchScope::Folder => "This folder",
            SearchScope::Subfolders => "Folder and subfolders",
            SearchScope::Account => "This account",
            SearchScope::All => "All mailboxes",
        }
    }
}

/// A parsed search query. Repeated field operators are ANDed; `received:` and
/// `size:` keep the last occurrence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// The free-text remainder, searched across the FTS index (headers and
    /// body). May contain quoted phrases.
    pub free_text: String,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: Vec<String>,
    pub body: Vec<String>,
    pub has_attachment: bool,
    /// `Some(true)` for `is:unread`, `Some(false)` for `is:read`.
    pub is_unread: Option<bool>,
    /// The follow-up flag (M-10), not Pin.
    pub is_flagged: bool,
    /// The `\Flagged` Pin (M-13).
    pub is_pinned: bool,
    /// Sanitised category keys (the `$Lookout-tag-<key>` suffix).
    pub category: Vec<String>,
    pub received: Option<ReceivedFilter>,
    pub size: Option<SizeFilter>,
    /// A folder-name filter; resolved to mailbox ids by the app.
    pub folder: Option<String>,
}

impl SearchQuery {
    /// Parses a raw query string - the inherent-method spelling of the free
    /// [`parse`] function, for call sites that prefer it.
    pub fn parse(input: &str) -> Self {
        parse(input)
    }

    /// Whether the query carries no operators and no free text.
    pub fn is_empty(&self) -> bool {
        self.free_text.trim().is_empty()
            && self.from.is_empty()
            && self.to.is_empty()
            && self.cc.is_empty()
            && self.subject.is_empty()
            && self.body.is_empty()
            && !self.has_attachment
            && self.is_unread.is_none()
            && !self.is_flagged
            && !self.is_pinned
            && self.category.is_empty()
            && self.received.is_none()
            && self.size.is_none()
            && self.folder.is_none()
    }

    /// The FTS5 `MATCH` expression for the text-bearing operators (free text,
    /// `from:` via the `sender` column, `subject:`, `body:`), or `None` when
    /// the query has no text component (a filter-only query).
    pub fn fts_match(&self) -> Option<String> {
        let mut clauses: Vec<String> = Vec::new();
        if !self.free_text.trim().is_empty() {
            if let Some(matched) = sanitize_fts_query(&self.free_text) {
                clauses.push(matched);
            }
        }
        for value in &self.from {
            clauses.push(format!("sender : {}", fts_phrase(value)));
        }
        for value in &self.subject {
            clauses.push(format!("subject : {}", fts_phrase(value)));
        }
        for value in &self.body {
            clauses.push(format!("body : {}", fts_phrase(value)));
        }
        if clauses.is_empty() {
            None
        } else {
            Some(clauses.join(" AND "))
        }
    }

    /// The SQL `WHERE` predicates and bound parameters for every operator that
    /// the FTS index does not cover. All columns live inside `messages.data`
    /// JSON, so each predicate is a `json_extract`/`json_each` expression.
    pub fn sql_predicates(&self) -> (Vec<String>, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();

        for (column, values) in [("$.to", &self.to), ("$.cc", &self.cc)] {
            for value in values {
                clauses.push(format!(
                    "EXISTS (SELECT 1 FROM json_each(messages.data, '{column}') \
                     WHERE json_extract(value, '$.address') LIKE ? ESCAPE '\\' \
                        OR json_extract(value, '$.name') LIKE ? ESCAPE '\\')"
                ));
                let like = format!("%{}%", escape_like(value));
                params.push(Value::Text(like.clone()));
                params.push(Value::Text(like));
            }
        }
        if self.has_attachment {
            clauses.push("json_extract(messages.data, '$.has_attachment') = 1".to_string());
        }
        match self.is_unread {
            Some(true) => clauses.push("NOT EXISTS (SELECT 1 FROM json_each(messages.data, '$.flags') WHERE value = 'Seen')".to_string()),
            Some(false) => clauses.push("EXISTS (SELECT 1 FROM json_each(messages.data, '$.flags') WHERE value = 'Seen')".to_string()),
            None => {}
        }
        if self.is_pinned {
            clauses.push("EXISTS (SELECT 1 FROM json_each(messages.data, '$.flags') WHERE value = 'Flagged')".to_string());
        }
        if self.is_flagged {
            clauses.push("EXISTS (SELECT 1 FROM json_each(messages.data, '$.keywords') WHERE value = ? OR value LIKE ?)".to_string());
            params.push(Value::Text(FLAG_KEYWORD.to_string()));
            params.push(Value::Text(format!("{FLAG_DUE_KEYWORD_PREFIX}%")));
            clauses.push("NOT EXISTS (SELECT 1 FROM json_each(messages.data, '$.keywords') WHERE value = ?)".to_string());
            params.push(Value::Text(FLAG_DONE_KEYWORD.to_string()));
        }
        for key in &self.category {
            clauses.push("EXISTS (SELECT 1 FROM json_each(messages.data, '$.keywords') WHERE value = ?)".to_string());
            params.push(Value::Text(tag_keyword(key)));
        }
        if let Some(received) = &self.received {
            if let Some(after) = received.after {
                clauses.push("CAST(strftime('%s', json_extract(messages.data, '$.date')) AS INTEGER) >= ?".to_string());
                params.push(Value::Integer(after.timestamp()));
            }
            if let Some(before) = received.before {
                clauses.push("CAST(strftime('%s', json_extract(messages.data, '$.date')) AS INTEGER) < ?".to_string());
                params.push(Value::Integer(before.timestamp()));
            }
        }
        if let Some(size) = &self.size {
            let op = match size.cmp {
                Cmp::Lt => "<",
                Cmp::Le => "<=",
                Cmp::Gt => ">",
                Cmp::Ge => ">=",
                Cmp::Eq => "=",
            };
            clauses.push(format!("json_extract(messages.data, '$.size') {op} ?"));
            params.push(Value::Integer(size.bytes as i64));
        }
        (clauses, params)
    }

    /// The IMAP `UID SEARCH` criterion string for the server fallback. `folder:`
    /// is not expressible (the mailbox is chosen by the SELECT) and
    /// `has:attachment` has no IMAP key, so both are left to
    /// [`SearchQuery::matches_summary`].
    pub fn to_imap_criteria(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.free_text.trim().is_empty() {
            parts.push(format!("TEXT {}", imap_quote(&self.free_text)));
        }
        for value in &self.from {
            parts.push(format!("FROM {}", imap_quote(value)));
        }
        for value in &self.to {
            parts.push(format!("TO {}", imap_quote(value)));
        }
        for value in &self.cc {
            parts.push(format!("CC {}", imap_quote(value)));
        }
        for value in &self.subject {
            parts.push(format!("SUBJECT {}", imap_quote(value)));
        }
        for value in &self.body {
            parts.push(format!("BODY {}", imap_quote(value)));
        }
        match self.is_unread {
            Some(true) => parts.push("UNSEEN".to_string()),
            Some(false) => parts.push("SEEN".to_string()),
            None => {}
        }
        if self.is_pinned {
            parts.push("FLAGGED".to_string());
        }
        if self.is_flagged {
            parts.push(format!("KEYWORD {}", imap_quote(FLAG_KEYWORD)));
            parts.push(format!("UNKEYWORD {}", imap_quote(FLAG_DONE_KEYWORD)));
        }
        for key in &self.category {
            parts.push(format!("KEYWORD {}", imap_quote(&tag_keyword(key))));
        }
        if let Some(received) = &self.received {
            // IMAP dates are day-granular, so a time-of-day bound rounds to the
            // enclosing day; `matches_summary` trims the excess.
            if let Some(after) = received.after {
                parts.push(format!("SINCE {}", imap_date(after)));
            }
            if let Some(before) = received.before {
                parts.push(format!("BEFORE {}", imap_date(before)));
            }
        }
        if let Some(size) = &self.size {
            match size.cmp {
                Cmp::Gt => parts.push(format!("LARGER {}", size.bytes)),
                Cmp::Ge => parts.push(format!("LARGER {}", size.bytes.saturating_sub(1))),
                Cmp::Lt => parts.push(format!("SMALLER {}", size.bytes)),
                Cmp::Le => parts.push(format!("SMALLER {}", size.bytes.saturating_add(1))),
                Cmp::Eq => {
                    parts.push(format!("LARGER {}", size.bytes.saturating_sub(1)));
                    parts.push(format!("SMALLER {}", size.bytes.saturating_add(1)));
                }
            }
        }
        parts.join(" ")
    }

    /// Re-checks the predicates on a fetched [`EmailSummary`] - the post-filter
    /// the server fallback applies, since IMAP can't express `has:attachment`,
    /// `category:` (a custom keyword mapping), or exact `to:`/`cc:` matching.
    /// Body and free-text terms aren't on the summary and are assumed to hold
    /// (the IMAP `TEXT`/`BODY` keys already matched them).
    pub fn matches_summary(&self, message: &EmailSummary) -> bool {
        for value in &self.from {
            if !addresses_contain(&message.from, value) {
                return false;
            }
        }
        for value in &self.to {
            if !addresses_contain(&message.to, value) {
                return false;
            }
        }
        for value in &self.cc {
            if !addresses_contain(&message.cc, value) {
                return false;
            }
        }
        for value in &self.subject {
            if !message.subject.as_deref().is_some_and(|subject| subject.to_lowercase().contains(&value.to_lowercase())) {
                return false;
            }
        }
        if self.has_attachment && !message.has_attachment {
            return false;
        }
        match self.is_unread {
            Some(true) if !message.is_unread() => return false,
            Some(false) if message.is_unread() => return false,
            _ => {}
        }
        if self.is_pinned && !message.is_pinned() {
            return false;
        }
        if self.is_flagged && !message.is_flagged_for_follow_up() {
            return false;
        }
        for key in &self.category {
            if !message.keywords.contains(&tag_keyword(key)) {
                return false;
            }
        }
        if let Some(received) = &self.received {
            if !received.matches(message.date) {
                return false;
            }
        }
        if let Some(size) = &self.size {
            if !size.matches(message.size) {
                return false;
            }
        }
        true
    }
}

impl std::fmt::Display for SearchQuery {
    /// Canonical, re-parseable form (so the search box, the refiner chips and
    /// the stored history all speak the same string).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        for value in &self.from {
            parts.push(format!("from:{}", quote_field(value)));
        }
        for value in &self.to {
            parts.push(format!("to:{}", quote_field(value)));
        }
        for value in &self.cc {
            parts.push(format!("cc:{}", quote_field(value)));
        }
        for value in &self.subject {
            parts.push(format!("subject:{}", quote_field(value)));
        }
        for value in &self.body {
            parts.push(format!("body:{}", quote_field(value)));
        }
        if self.has_attachment {
            parts.push("has:attachment".to_string());
        }
        match self.is_unread {
            Some(true) => parts.push("is:unread".to_string()),
            Some(false) => parts.push("is:read".to_string()),
            None => {}
        }
        if self.is_flagged {
            parts.push("is:flagged".to_string());
        }
        if self.is_pinned {
            parts.push("is:pinned".to_string());
        }
        for key in &self.category {
            parts.push(format!("category:{}", quote_field(key)));
        }
        if let Some(received) = &self.received {
            parts.push(format!("received:{}", received.label));
        }
        if let Some(size) = &self.size {
            parts.push(format!("size:{}", size.label));
        }
        if let Some(folder) = &self.folder {
            parts.push(format!("folder:{}", quote_field(folder)));
        }
        if !self.free_text.trim().is_empty() {
            parts.push(self.free_text.trim().to_string());
        }
        write!(f, "{}", parts.join(" "))
    }
}

/// Parses a raw query string.
pub fn parse(input: &str) -> SearchQuery {
    let mut query = SearchQuery::default();
    let mut free: Vec<String> = Vec::new();
    for token in tokenize(input) {
        let Some((prefix, value)) = split_prefix(&token) else {
            free.push(token);
            continue;
        };
        match prefix.as_str() {
            "from" if !value.is_empty() => query.from.push(value),
            "to" if !value.is_empty() => query.to.push(value),
            "cc" if !value.is_empty() => query.cc.push(value),
            "subject" if !value.is_empty() => query.subject.push(value),
            "body" if !value.is_empty() => query.body.push(value),
            "has" => {
                if value.eq_ignore_ascii_case("attachment") {
                    query.has_attachment = true;
                } else {
                    free.push(token);
                }
            }
            "is" => match value.to_ascii_lowercase().as_str() {
                "unread" => query.is_unread = Some(true),
                "read" => query.is_unread = Some(false),
                "flagged" => query.is_flagged = true,
                "pinned" => query.is_pinned = true,
                _ => {}
            },
            "category" => {
                let key = sanitize_tag_key(&value);
                if !key.is_empty() {
                    query.category.push(key);
                }
            }
            "received" => {
                if let Some(received) = parse_received(&value) {
                    query.received = Some(received);
                }
            }
            "size" => {
                if let Some(size) = parse_size(&value) {
                    query.size = Some(size);
                }
            }
            "folder" if !value.is_empty() => query.folder = Some(value),
            _ => free.push(token),
        }
    }
    query.free_text = free.join(" ");
    query
}

/// A `received:` date predicate: a half-open UTC window `[after, before)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFilter {
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    /// The original token value, kept for [`SearchQuery`]'s `Display`.
    pub label: String,
}

impl ReceivedFilter {
    pub fn matches(&self, date: DateTime<Utc>) -> bool {
        self.after.is_none_or(|after| date >= after) && self.before.is_none_or(|before| date < before)
    }
}

/// A `size:` predicate in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeFilter {
    pub cmp: Cmp,
    pub bytes: u64,
    /// The original token value, kept for [`SearchQuery`]'s `Display`.
    pub label: String,
}

impl SizeFilter {
    pub fn matches(&self, size: u32) -> bool {
        let size = size as u64;
        match self.cmp {
            Cmp::Lt => size < self.bytes,
            Cmp::Le => size <= self.bytes,
            Cmp::Gt => size > self.bytes,
            Cmp::Ge => size >= self.bytes,
            Cmp::Eq => size == self.bytes,
        }
    }
}

/// A size comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

// ---- parsing helpers -------------------------------------------------------

/// Splits `input` into tokens, keeping a `"`-quoted run (spaces included) as
/// one token, with the quotes retained.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Splits an operator token into its (lowercased) prefix and (unquoted) value.
/// Returns `None` for a token with no `:` or a prefix that isn't a plain word,
/// so such tokens stay free text verbatim.
fn split_prefix(token: &str) -> Option<(String, String)> {
    let idx = token.find(':')?;
    let prefix = token[..idx].to_ascii_lowercase();
    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((prefix, strip_quotes(&token[idx + 1..])))
}

/// Removes a single pair of surrounding double quotes, if present.
fn strip_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn split_op(value: &str) -> (Op, &str) {
    if let Some(rest) = value.strip_prefix(">=") {
        (Op::Ge, rest)
    } else if let Some(rest) = value.strip_prefix("<=") {
        (Op::Le, rest)
    } else if let Some(rest) = value.strip_prefix('>') {
        (Op::Gt, rest)
    } else if let Some(rest) = value.strip_prefix('<') {
        (Op::Lt, rest)
    } else if let Some(rest) = value.strip_prefix('=') {
        (Op::Eq, rest)
    } else {
        (Op::None, value)
    }
}

fn parse_received(value: &str) -> Option<ReceivedFilter> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    if let Some((after, before)) = relative_range(&lower) {
        return Some(ReceivedFilter { after, before, label: lower });
    }
    let (op, rest) = split_op(value);
    let date = parse_date(rest.trim())?;
    let start = day_start(date);
    let build = |after: Option<DateTime<Utc>>, before: Option<DateTime<Utc>>| ReceivedFilter {
        after,
        before,
        label: value.to_string(),
    };
    match op {
        Op::Ge => Some(build(Some(start), None)),
        Op::Gt => Some(build(Some(day_start(date.succ_opt()?)), None)),
        Op::Le => Some(build(None, Some(day_start(date.succ_opt()?)))),
        Op::Lt => Some(build(None, Some(start))),
        Op::Eq | Op::None => Some(build(Some(start), Some(day_start(date.succ_opt()?)))),
    }
}

fn parse_size(value: &str) -> Option<SizeFilter> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (op, rest) = split_op(value);
    let rest = rest.trim();
    let digits_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    let number: u64 = rest[..digits_end].parse().ok()?;
    let unit = rest[digits_end..].trim().to_ascii_lowercase();
    let multiplier: u64 = match unit.as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024 * 1024,
        "g" | "gb" => 1024 * 1024 * 1024,
        _ => return None,
    };
    let bytes = number.checked_mul(multiplier)?;
    let cmp = match op {
        Op::Gt => Cmp::Gt,
        Op::Ge => Cmp::Ge,
        Op::Lt => Cmp::Lt,
        Op::Le => Cmp::Le,
        Op::Eq | Op::None => Cmp::Eq,
    };
    Some(SizeFilter {
        cmp,
        bytes,
        label: value.to_string(),
    })
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

fn day_start(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0).expect("midnight is valid").and_utc()
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).expect("valid month start")
}

fn first_of_next_month(date: NaiveDate) -> NaiveDate {
    let (year, month) = (date.year(), date.month());
    if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1).expect("valid month start")
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1).expect("valid month start")
    }
}

/// A half-open UTC date window `[after, before)`, either side open.
type DateWindow = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Resolves a relative `received:` keyword to a `[after, before)` UTC window,
/// using UTC day boundaries.
fn relative_range(key: &str) -> Option<DateWindow> {
    let today = Utc::now().date_naive();
    let range = match key {
        "today" => (Some(day_start(today)), Some(day_start(today.succ_opt()?))),
        "yesterday" => (Some(day_start(today.pred_opt()?)), Some(day_start(today))),
        "lastweek" | "last7days" => (Some(day_start(today - Duration::days(7))), Some(day_start(today.succ_opt()?))),
        "thismonth" => (Some(day_start(first_of_month(today))), Some(day_start(first_of_next_month(today)))),
        "lastmonth" => {
            let first = first_of_month(today);
            let previous = first.pred_opt()?;
            (Some(day_start(first_of_month(previous))), Some(day_start(first)))
        }
        "thisyear" => (
            Some(day_start(NaiveDate::from_ymd_opt(today.year(), 1, 1)?)),
            Some(day_start(NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)?)),
        ),
        "lastyear" => (
            Some(day_start(NaiveDate::from_ymd_opt(today.year() - 1, 1, 1)?)),
            Some(day_start(NaiveDate::from_ymd_opt(today.year(), 1, 1)?)),
        ),
        _ => return None,
    };
    Some(range)
}

fn addresses_contain(addresses: &[EmailAddress], needle: &str) -> bool {
    let needle = needle.to_lowercase();
    addresses
        .iter()
        .any(|address| address.address.to_lowercase().contains(&needle) || address.name.as_deref().is_some_and(|name| name.to_lowercase().contains(&needle)))
}

/// The FTS5 phrase for a column filter: quoted, with embedded quotes flattened
/// so a value can never break out (mirroring `sanitize_fts_query`).
fn fts_phrase(value: &str) -> String {
    format!("\"{}\"", value.replace('"', " "))
}

/// Quotes a field value for [`SearchQuery`]'s `Display`.
fn quote_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', " "))
}

/// Escapes a value for a SQL `LIKE` pattern (paired with `ESCAPE '\'`).
fn escape_like(value: &str) -> String {
    value.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Quotes and escapes an IMAP search argument (RFC 3501 §9, shared with the
/// session's other `SEARCH` deployments).
pub(crate) fn imap_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Formats a date as an IMAP `date` (RFC 3501 §9), e.g. `14-Oct-2026`.
fn imap_date(date: DateTime<Utc>) -> String {
    date.format("%d-%b-%Y").to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    None,
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{Importance, MessageSensitivity, SystemFlagBit, ThreadKey, Uid};
    use std::collections::BTreeSet;

    fn summary() -> EmailSummary {
        EmailSummary {
            uid: Uid(1),
            mailbox: lookout_core::MailboxId::new(&lookout_core::AccountId("a".into()), "INBOX"),
            message_id: Some("<m@example.com>".into()),
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey("t".into()),
            subject: Some("Quarterly report".into()),
            from: vec![EmailAddress {
                name: Some("Ada Lovelace".into()),
                address: "ada@example.com".into(),
            }],
            to: vec![EmailAddress::new("team@example.com")],
            cc: Vec::new(),
            date: "2026-06-15T10:00:00Z".parse().unwrap(),
            flags: BTreeSet::from([SystemFlagBit::Flagged]),
            keywords: BTreeSet::from([tag_keyword("work")]),
            size: 2 * 1024 * 1024,
            has_attachment: true,
            has_calendar: false,
            preview: None,
            importance: Importance::Normal,
            sensitivity: MessageSensitivity::Normal,
            list_id: None,
            structure: None,
        }
    }

    #[test]
    fn parses_fields_and_free_text() {
        let q = parse("from:ada to:team subject:\"q3 report\" has:attachment is:unread hello world");
        assert_eq!(q.from, vec!["ada"]);
        assert_eq!(q.to, vec!["team"]);
        assert_eq!(q.subject, vec!["q3 report"]);
        assert!(q.has_attachment);
        assert_eq!(q.is_unread, Some(true));
        assert_eq!(q.free_text, "hello world");
    }

    #[test]
    fn unknown_prefixes_stay_free_text() {
        let q = parse("http://example.com foo:bar plain");
        assert!(q.from.is_empty());
        assert_eq!(q.free_text, "http://example.com foo:bar plain");
    }

    #[test]
    fn unknown_is_value_is_dropped() {
        let q = parse("is:bogus is:pinned");
        assert!(q.is_pinned);
        assert_eq!(q.free_text, "");
    }

    #[test]
    fn categories_are_sanitised_to_keys() {
        let q = parse("category:\"Work Items\"");
        assert_eq!(q.category, vec!["work-items"]);
    }

    #[test]
    fn flags_are_distinct_from_pins() {
        let q = parse("is:flagged");
        assert!(q.is_flagged && !q.is_pinned);
        let q = parse("is:pinned");
        assert!(q.is_pinned && !q.is_flagged);
    }

    #[test]
    fn absolute_received_bounds() {
        let q = parse("received:>=2026-01-01");
        let r = q.received.unwrap();
        assert_eq!(r.after, Some("2026-01-01T00:00:00Z".parse().unwrap()));
        assert_eq!(r.before, None);

        let q = parse("received:<2026-02-01");
        let r = q.received.unwrap();
        assert_eq!(r.after, None);
        assert_eq!(r.before, Some("2026-02-01T00:00:00Z".parse().unwrap()));

        let q = parse("received:2026-03-05");
        let r = q.received.unwrap();
        assert!(r.matches("2026-03-05T23:59:59Z".parse().unwrap()));
        assert!(!r.matches("2026-03-06T00:00:00Z".parse().unwrap()));
    }

    #[test]
    fn relative_received_includes_now() {
        for key in ["today", "last7days"] {
            let q = parse(&format!("received:{key}"));
            let r = q.received.unwrap();
            assert!(r.matches(Utc::now()), "{key} should include now");
        }
    }

    #[test]
    fn size_parsing_and_matching() {
        let q = parse("size:>1MB");
        let s = q.size.unwrap();
        assert_eq!(s.cmp, Cmp::Gt);
        assert_eq!(s.bytes, 1024 * 1024);
        assert!(s.matches(2 * 1024 * 1024));
        assert!(!s.matches(512 * 1024));

        let q = parse("size:<=10KB");
        let s = q.size.unwrap();
        assert_eq!(s.cmp, Cmp::Le);
        assert_eq!(s.bytes, 10 * 1024);
        assert!(s.matches(10 * 1024));
        assert!(!s.matches(10 * 1024 + 1));
    }

    #[test]
    fn display_round_trips() {
        for input in [
            "from:ada to:team subject:\"q3 report\" has:attachment is:unread category:work received:>=2026-01-01 size:>1MB free words",
            "is:pinned is:flagged body:invoice",
        ] {
            let parsed = parse(input);
            let reparsed = parse(&parsed.to_string());
            assert_eq!(parsed, reparsed, "round trip for {input:?}");
        }
    }

    #[test]
    fn fts_match_covers_text_but_not_filters() {
        let q = parse("has:attachment subject:report");
        let m = q.fts_match().unwrap();
        assert!(m.contains("subject : \"report\""));
        // Filter-only query has no FTS component.
        assert!(parse("is:unread has:attachment").fts_match().is_none());
        assert!(parse("hello world").fts_match().is_some());
    }

    #[test]
    fn imap_criteria_translates_operators() {
        let criteria = parse("from:ada is:unread received:>=2026-01-01 size:>1MB").to_imap_criteria();
        assert!(criteria.contains("FROM \"ada\""));
        assert!(criteria.contains("UNSEEN"));
        assert!(criteria.contains("SINCE 01-Jan-2026"));
        assert!(criteria.contains("LARGER 1048576"));
        // has:attachment has no IMAP key.
        assert!(!parse("has:attachment").to_imap_criteria().contains("ATTACHMENT"));
    }

    #[test]
    fn matches_summary_evaluates_meanings() {
        let message = summary();
        assert!(parse("from:ada has:attachment category:work size:>1MB received:>=2026-01-01").matches_summary(&message));
        assert!(parse("is:unread").matches_summary(&message));
        assert!(!parse("is:read").matches_summary(&message));
        assert!(parse("is:pinned subject:report").matches_summary(&message));
        assert!(!parse("category:personal").matches_summary(&message));
        assert!(!parse("to:nobody").matches_summary(&message));
    }

    #[test]
    fn sql_predicates_bind_parameters() {
        let (clauses, params) = parse("to:team has:attachment is:unread category:work").sql_predicates();
        assert_eq!(clauses.len(), 4);
        assert_eq!(params.len(), 3);
    }
}
