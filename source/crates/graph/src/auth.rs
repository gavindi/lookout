/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use async_trait::async_trait;

use crate::error::Result;

/// Supplies Microsoft Graph access tokens and manages incremental consent.
///
/// Deliberately the platform seam, mirroring `lookout-dav`'s
/// `CalendarCredentialProvider` and `lookout-mail`'s `CredentialProvider`: the
/// app implements this against `microsoft_oauth.rs` (`TokenAudience::Graph`),
/// so this crate stays free of OAuth, keyring and browser dependencies.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    /// A cached-or-refreshed Graph access token. Returns
    /// [`crate::GraphError::ConsentRequired`] when the account hasn't granted
    /// Graph access yet, so the caller can prompt rather than fail silently.
    async fn graph_token(&self) -> Result<String>;

    /// Ensures `scopes` are granted, running the interactive flow if needed.
    /// Called once before a feature's first request; an implementation may
    /// request a superset of what it already holds.
    async fn ensure_scopes(&self, scopes: &[&str]) -> Result<()>;

    /// Drops the cached Graph access token after the service rejected it
    /// (`401`). The next [`Self::graph_token`] call then refreshes from the
    /// stored refresh token. Defaulted to a no-op so test doubles and
    /// providers without an access-token cache need no change.
    async fn invalidate(&self) {}
}
