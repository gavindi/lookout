//! Mirrors Lookout's pin/unpin action to Outlook's own MAPI-property pin on
//! Microsoft 365 accounts, via the Microsoft Graph API.
//!
//! Outlook's "Pin" feature isn't an IMAP flag - Exchange encodes it as the
//! `PR_RENEW_TIME` MAPI property (tag `0x0F01`, type `PT_SYSTIME`) set to a
//! far-future date, which folder views sort by to force the message to the
//! top regardless of its real date. IMAP has no way to reach that property
//! at all (`STORE` only touches system/keyword flags), so this module talks
//! to Graph's extended-properties API instead - the only reachable transport
//! now that EWS is being retired for Exchange Online. It rides the shared
//! `lookout_graph::mail` transport (item 5.4) for the `internetMessageId`
//! lookup and the `PATCH`.
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
use chrono::{DateTime, SecondsFormat, Utc};
use lookout_graph::{GraphError, MailClient};
use serde_json::json;

use crate::graph_account::{GraphAccount, GraphFeatureClient};

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

impl From<GraphError> for GraphPinError {
    fn from(error: GraphError) -> Self {
        match error {
            GraphError::ConsentRequired(_) => GraphPinError::ConsentRequired,
            other => GraphPinError::Http(other.to_string()),
        }
    }
}

/// One Microsoft 365 account's Graph pin-mirroring client. Cheap to
/// construct - holds a [`MailClient`] over the account's shared
/// [`GraphAccount`], whose OAuth instance the IMAP/SMTP session uses too.
pub struct GraphPinClient {
    account: GraphAccount,
    mail: MailClient,
}

impl GraphPinClient {
    pub fn new(account: GraphAccount) -> Self {
        GraphPinClient {
            mail: MailClient::new(account.graph().clone()),
            account,
        }
    }

    /// Runs the interactive consent flow for Config's "Sync pins with
    /// Outlook" action.
    pub async fn ensure_consent(&self) -> Result<(), String> {
        self.account.ensure_outlook_consent().await
    }

    /// Sets or clears `PR_RENEW_TIME` on the Graph message matching
    /// `message_id` (an RFC 822 `Message-ID`, angle brackets included).
    /// `original_date` is written back on unpin, matching Outlook's own
    /// "reset to the item's real date" unpin behavior.
    pub async fn set_pinned(&self, message_id: &str, original_date: DateTime<Utc>, pinned: bool) -> Result<(), GraphPinError> {
        // The same `internetMessageId` lookup the mail-property mirror uses
        // (item 5.4), so the two share one resolver.
        let graph_id = self.mail.resolve_message_id(message_id).await?.ok_or(GraphPinError::MessageNotFound)?;

        let value = if pinned {
            FAR_FUTURE_RENEW_TIME.to_string()
        } else {
            original_date.to_rfc3339_opts(SecondsFormat::Secs, true)
        };
        let body = json!({
            "singleValueExtendedProperties": [
                { "id": "SystemTime 0x0F01", "value": value }
            ]
        });
        self.mail.patch_message(&graph_id, &body).await?;
        Ok(())
    }
}

impl GraphFeatureClient for GraphPinClient {
    fn account(&self) -> &GraphAccount {
        &self.account
    }
}
