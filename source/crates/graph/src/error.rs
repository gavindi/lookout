/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::time::Duration;

/// Everything that can go wrong talking to Microsoft Graph, normalised so
/// feature code can branch on cause without matching HTTP status codes itself.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// The HTTP request itself failed (DNS, TLS, connection, body read).
    #[error("Graph request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A successful response whose body wasn't the JSON shape expected.
    #[error("could not decode the Graph response: {0}")]
    Decode(#[from] serde_json::Error),
    /// The account needs interactive consent for the requested scopes before
    /// this feature can run. The UI turns this into a "Connect to Microsoft
    /// 365" prompt.
    #[error("consent required for: {0}")]
    ConsentRequired(String),
    /// The token provider couldn't produce a token (network, revoked grant,
    /// misconfiguration). Retryable in the sense that a later attempt may
    /// succeed after the user signs in again.
    #[error("Graph token error: {0}")]
    Token(String),
    /// 401: the access token was rejected. A fresh token (re-fetched from the
    /// provider on the next request) may succeed; the caller usually just
    /// surfaces "signed out".
    #[error("Graph request was unauthorized")]
    Unauthorized,
    /// Any other non-success status, with Graph's own error body and
    /// `request-id` (for support tickets) kept verbatim.
    #[error("Graph error {status}: {message}")]
    Status { status: u16, message: String, request_id: Option<String> },
    /// The request stayed throttled through every retry. `retry_after` is the
    /// last `Retry-After` Graph asked for, if it sent one.
    #[error("Graph throttled the request after {attempts} attempts")]
    Throttled { attempts: u32, retry_after: Option<Duration> },
    /// The event's recurrence rule can't be expressed in Graph's structured
    /// recurrence model. Returned instead of silently writing the event
    /// without recurrence (the editor normally catches this first).
    #[error("Microsoft 365 can't store this recurrence rule: {0}")]
    UnsupportedRecurrence(String),
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, GraphError>;
