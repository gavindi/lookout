//! The Microsoft 365 company-directory (GAL) provider for recipient
//! autocomplete and People search (OUTLOOK_PARITY.md item 4.7.3 / P-11).
//!
//! This is a [`DirectoryBackend`] over `lookout_graph::contacts`:
//! `search_directory` (`/users?$search=`) is the real tenant-wide GAL, with
//! `search_people` (`/me/people`) as the fallback when it fails or answers
//! nothing - the relevance list can carry guests and external contacts the
//! user list doesn't, and it works on tenants where `/users?$search=` is
//! policy-blocked. Every failure degrades to no hits, never an error (§10).
//!
//! The directory scopes (`User.ReadBasic.All`, `People.Read`) deliberately sit
//! outside the default "Sync with Outlook" consent, so an account contributes
//! hits only when **both** gates hold: the user's per-account opt-in
//! (`AppConfig::directory_accounts`, its Settings → People switch is item
//! 4.7.9) and an in-place Graph consent
//! ([`GraphDirectoryBackend::consented`]). [`GraphDirectoryBackend::enabled`]
//! is the combined check the 4.7.6 wiring uses when it assembles the running
//! backends.
//!
//! [`GraphDirectoryBackend::ensure_consent`] runs the *interactive* OAuth flow
//! (it opens a browser), so it is only ever called from a user-initiated
//! action - the opt-in switch. [`DirectoryBackend::search`] never calls it: a
//! keystroke must not surprise-launch a browser, so an account without the
//! grant simply contributes nothing until the user enables it.
//!
//! Wired into the composer/attendee fields by `directory::directory_source`
//! (item 4.7.6).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;

use async_trait::async_trait;
use lookout_core::AccountId;
use lookout_graph::{ContactsClient, DirectoryPerson, GraphClient};

use crate::app_config::AppConfig;
use crate::directory::DirectoryBackend;
use crate::microsoft_oauth::MicrosoftOAuth;

/// One Microsoft 365 account's Graph directory provider. Cheap to construct -
/// holds a [`ContactsClient`] over an independent `MicrosoftOAuth` (the mail
/// session and the other Graph feature clients each hold their own; they all
/// safely share the same on-disk token file through its atomic save).
pub(crate) struct GraphDirectoryBackend {
    account_id: AccountId,
    oauth: Arc<MicrosoftOAuth>,
    contacts: ContactsClient,
}

impl GraphDirectoryBackend {
    /// Builds the provider for one Microsoft 365 account. Constructing it
    /// touches no network and no token file - whether it is actually offered
    /// to a [`crate::directory::DirectorySource`] is [`Self::enabled`]'s call.
    pub(crate) fn new(account_id: AccountId) -> Self {
        let oauth = Arc::new(MicrosoftOAuth::new(account_id.clone()));
        GraphDirectoryBackend {
            account_id,
            contacts: ContactsClient::new(GraphClient::new(oauth.clone())),
            oauth,
        }
    }

    /// A provider over an explicit contacts client, for tests driving a
    /// wiremock base URL.
    #[cfg(test)]
    fn with_client(account_id: AccountId, contacts: ContactsClient) -> Self {
        let oauth = Arc::new(MicrosoftOAuth::new(account_id.clone()));
        GraphDirectoryBackend { account_id, oauth, contacts }
    }

    /// Synchronous, disk-only check for the account's "Sync with Outlook"
    /// Graph consent - the second of [`Self::enabled`]'s two gates.
    pub(crate) fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Whether the account currently contributes directory hits: the user has
    /// opted the account in (`AppConfig::directory_accounts`) **and** its
    /// Graph consent is in place. The wiring checks this when it assembles the
    /// enabled backends; a shared mailbox or an account that never consented
    /// fails it naturally, at no network cost.
    pub(crate) fn enabled(&self, config: &AppConfig) -> bool {
        config.directory_enabled_for(&self.account_id) && self.consented()
    }

    /// Runs the interactive consent flow for the directory scopes
    /// (`User.ReadBasic.All`, `People.Read`), which are outside the default
    /// combined consent. Only ever called from a user-initiated action - the
    /// opt-in switch (item 4.7.9) - never from [`DirectoryBackend::search`].
    /// Unused until that switch lands, hence the targeted allow.
    #[allow(dead_code)]
    pub(crate) async fn ensure_consent(&self) -> Result<(), String> {
        self.contacts.ensure_directory_consent().await.map_err(|error| error.to_string())
    }

    /// The `/me/people` fallback, degrading an unreachable or unauthorised
    /// relevance list to no hits like the primary search does.
    async fn people_fallback(&self, query: &str, limit: usize) -> Vec<DirectoryPerson> {
        match self.contacts.search_people(query, limit).await {
            Ok(hits) => hits,
            Err(error) => {
                tracing::debug!(account = %self.account_id, "Graph people search failed: {error}");
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl DirectoryBackend for GraphDirectoryBackend {
    /// The tenant user list first, falling back to the relevance-ranked people
    /// list when `/users?$search=` errors or finds nobody. Never errors: a
    /// denied scope (`ConsentRequired`), throttling or a dead service all
    /// come back as an empty list, the recipient popover simply shows stored
    /// contacts only.
    async fn search(&self, query: &str, limit: usize) -> Vec<DirectoryPerson> {
        match self.contacts.search_directory(query, limit).await {
            Ok(hits) if !hits.is_empty() => hits,
            Ok(_) => self.people_fallback(query, limit).await,
            Err(error) => {
                tracing::debug!(account = %self.account_id, "Graph directory search failed: {error}");
                self.people_fallback(query, limit).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_graph::TokenProvider;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> lookout_graph::Result<String> {
            Ok("test-token".to_string())
        }

        async fn ensure_scopes(&self, _scopes: &[&str]) -> lookout_graph::Result<()> {
            Ok(())
        }
    }

    fn backend(server: &MockServer) -> GraphDirectoryBackend {
        let client = GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri()));
        GraphDirectoryBackend::with_client(AccountId("account_1".into()), ContactsClient::new(client))
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    #[test]
    fn search_returns_directory_users_without_falling_back() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/users"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [{ "id": "u1", "displayName": "Grace Hopper", "mail": "grace@example.com", "jobTitle": "Admiral" }]
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1.0/me/people"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
                .expect(0)
                .mount(&server)
                .await;

            let hits = backend(&server).search("grace", 8).await;
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].address, "grace@example.com");
            assert_eq!(hits[0].name.as_deref(), Some("Grace Hopper"));
            assert_eq!(hits[0].title.as_deref(), Some("Admiral"));
        });
    }

    #[test]
    fn search_falls_back_to_people_when_the_directory_fails() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/users"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1.0/me/people"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [{ "id": "u2", "displayName": "Ada King", "scoredEmailAddresses": [{ "address": "ada@example.com" }] }]
                })))
                .expect(1)
                .mount(&server)
                .await;

            let hits = backend(&server).search("ada", 8).await;
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].address, "ada@example.com");
            assert_eq!(hits[0].name.as_deref(), Some("Ada King"));
        });
    }

    #[test]
    fn search_falls_back_when_the_directory_finds_nobody() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/users"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1.0/me/people"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [{ "id": "u3", "displayName": "Guest", "scoredEmailAddresses": [{ "address": "guest@partner.example" }] }]
                })))
                .expect(1)
                .mount(&server)
                .await;

            let hits = backend(&server).search("guest", 8).await;
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].address, "guest@partner.example");
        });
    }

    #[test]
    fn search_degrades_to_no_hits_when_everything_fails() {
        block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1.0/users"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1.0/me/people"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;

            let hits = backend(&server).search("anyone", 8).await;
            assert!(hits.is_empty());
        });
    }
}
