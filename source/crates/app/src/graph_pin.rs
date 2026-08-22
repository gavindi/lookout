//! Mirrors Lookout's pin/unpin action to Outlook's own MAPI-property pin on
//! Microsoft 365 accounts, via the Microsoft Graph API.
//!
//! Outlook's "Pin" feature isn't an IMAP flag - Exchange encodes it as the
//! `PR_RENEW_TIME` MAPI property (tag `0x0F01`, type `PT_SYSTIME`) set to a
//! far-future date, which folder views sort by to force the message to the
//! top regardless of its real date. IMAP has no way to reach that property
//! at all (`STORE` only touches system/keyword flags), so this module talks
//! to Graph's extended-properties API instead - the only reachable transport
//! now that EWS is being retired for Exchange Online.
//!
//! This is a **write-only mirror**: Lookout's own pinned/unpinned state stays
//! driven by the IMAP `\Flagged` flag exactly as before (see
//! `window.rs::optimistic_toggle_pinned`) on every account, Microsoft 365
//! included. Reading pins made in real Outlook back into Lookout would need
//! a second sync subsystem (polling Graph's extended properties across the
//! whole mailbox) and is out of scope here.
//!
//! `PR_RENEW_TIME_2` (`0x0F02`), Outlook's secondary pin-transaction
//! property, is also out of scope for the same reason it's hard to unpin
//! faithfully: Graph's extended-properties API has no clean way to delete a
//! single property, only to overwrite the whole array, so there's no way to
//! reproduce Outlook's "delete 0x0F02 entirely on unpin" semantics. Ship
//! with `PR_RENEW_TIME` alone; add `PR_RENEW_TIME_2` later only if a live
//! check against real Outlook shows the pin icon needs it.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::rc::Rc;

use chrono::{DateTime, SecondsFormat, Utc};
use lookout_core::AccountId;

use crate::microsoft_oauth::{GraphAuthError, MicrosoftOAuth};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// The `PR_RENEW_TIME` sentinel Exchange itself uses for a pinned message -
/// far enough in the future that date-sorted views always put it first.
const FAR_FUTURE_RENEW_TIME: &str = "4500-01-09T00:00:00Z";

/// Why mirroring a pin to Graph failed. All are non-fatal to the caller: the
/// IMAP `\Flagged` toggle this rides alongside has already happened
/// regardless (see `window.rs::mirror_pin_to_graph`), so every variant here
/// just gets logged, never surfaced as a user-facing pin failure.
pub enum GraphPinError {
    /// This account hasn't run `ensure_graph_consent` (or the grant no
    /// longer works). Expected and quiet - most Microsoft 365 accounts will
    /// never opt in.
    ConsentRequired,
    /// No message on the server matched the local `Message-ID` - e.g. it
    /// hasn't finished landing in the mailbox from Graph's point of view
    /// yet.
    MessageNotFound,
    Http(String),
}

/// One Microsoft 365 account's Graph pin-mirroring client. Cheap to
/// construct - holds its own `reqwest::Client` and an independent
/// `MicrosoftOAuth` (separate from the one driving the IMAP/SMTP session;
/// both safely share the same on-disk token file via its atomic save).
pub struct GraphPinClient {
    oauth: Rc<MicrosoftOAuth>,
    http: reqwest::Client,
}

impl GraphPinClient {
    pub fn new(account_id: AccountId) -> Self {
        GraphPinClient {
            oauth: Rc::new(MicrosoftOAuth::new(account_id)),
            http: reqwest::Client::new(),
        }
    }

    /// Synchronous, disk-only check for Config UI.
    pub fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Runs the interactive consent flow for Config's "Sync pins with
    /// Outlook" action.
    pub async fn ensure_consent(&self) -> Result<(), String> {
        self.oauth.ensure_graph_consent().await
    }

    /// Sets or clears `PR_RENEW_TIME` on the Graph message matching
    /// `message_id` (an RFC 822 `Message-ID`, angle brackets included).
    /// `original_date` is written back on unpin, matching Outlook's own
    /// "reset to the item's real date" unpin behavior.
    pub async fn set_pinned(&self, message_id: &str, original_date: DateTime<Utc>, pinned: bool) -> Result<(), GraphPinError> {
        let token = self.oauth.graph_access_token().await.map_err(|e| match e {
            GraphAuthError::ConsentRequired => GraphPinError::ConsentRequired,
            GraphAuthError::Transient(msg) => GraphPinError::Http(msg),
        })?;

        let graph_id = self.resolve_message_id(&token, message_id).await?;

        let value = if pinned {
            FAR_FUTURE_RENEW_TIME.to_string()
        } else {
            original_date.to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let body = serde_json::json!({
            "singleValueExtendedProperties": [
                { "id": "SystemTime 0x0F01", "value": value }
            ]
        });
        let resp = self
            .http
            .patch(format!("{GRAPH_BASE}/me/messages/{graph_id}"))
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| GraphPinError::Http(format!("Graph pin update failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(GraphPinError::Http(format!("Graph returned {status}: {text}")));
        }
        Ok(())
    }

    /// Resolves an RFC 822 `Message-ID` to the Graph message id needed for
    /// the extended-property write. No extra IMAP round trip: the caller
    /// already has `EmailSummary::message_id` from the standard `ENVELOPE`
    /// fetch.
    async fn resolve_message_id(&self, token: &str, message_id: &str) -> Result<String, GraphPinError> {
        // OData string-literal escaping: only the quote character needs
        // doubling. The angle brackets, `@`, etc. that RFC 822 Message-IDs
        // contain are fine inside a quoted literal; `reqwest`'s `.query()`
        // percent-encodes the whole filter value for the URL.
        let escaped = message_id.replace('\'', "''");
        let filter = format!("internetMessageId eq '{escaped}'");
        let url = format!("{GRAPH_BASE}/me/messages?$filter={}&$select=id", percent_encode(&filter));
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| GraphPinError::Http(format!("Graph message lookup failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(GraphPinError::Http(format!("Graph returned {status}: {text}")));
        }

        #[derive(serde::Deserialize)]
        struct Page {
            value: Vec<IdOnly>,
        }
        #[derive(serde::Deserialize)]
        struct IdOnly {
            id: String,
        }

        let page: Page = resp
            .json()
            .await
            .map_err(|e| GraphPinError::Http(format!("couldn't parse Graph message lookup response: {e}")))?;
        page.value.into_iter().next().map(|m| m.id).ok_or(GraphPinError::MessageNotFound)
    }
}

/// RFC 3986 unreserved characters pass through; everything else becomes
/// `%XX`. Same convention as `microsoft_oauth.rs`/`google_tasks.rs` -
/// `reqwest`'s own `.query()` needs the `query` Cargo feature this
/// workspace's `reqwest` dependency doesn't enable, so query strings here are
/// built by hand.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("internetMessageId eq '<a@b>'"), "internetMessageId%20eq%20%27%3Ca%40b%3E%27");
    }
}
