//! One Microsoft 365 account's Graph identity, shared by every Graph feature
//! client (pin, property and category mirroring, rules, automatic replies,
//! calendar sharing, rooms and the directory).
//!
//! Each feature used to build its own `MicrosoftOAuth` and `GraphClient`, and
//! carried its own copy of the consent gate. They now take a [`GraphAccount`],
//! whose OAuth half is the account's process-wide
//! [`MicrosoftOAuth::for_account`] instance, so every feature refreshes under
//! the same lock and reads the same in-memory consent flags.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;

use lookout_core::AccountId;
use lookout_graph::{GraphClient, GraphFeature};

use crate::microsoft_oauth::MicrosoftOAuth;

/// The OAuth instance and Graph transport for one Microsoft 365 account.
/// Cheap to clone: both halves are shared handles.
#[derive(Clone)]
pub struct GraphAccount {
    oauth: Arc<MicrosoftOAuth>,
    graph: GraphClient,
}

impl GraphAccount {
    pub fn new(account_id: &AccountId) -> Self {
        let oauth = MicrosoftOAuth::for_account(account_id);
        GraphAccount {
            graph: GraphClient::new(oauth.clone()),
            oauth,
        }
    }

    /// An account over an explicit transport, for tests driving a wiremock
    /// base URL. The consent gate still reads the account's real instance.
    #[cfg(test)]
    pub fn with_client(account_id: &AccountId, graph: GraphClient) -> Self {
        GraphAccount {
            oauth: MicrosoftOAuth::for_account(account_id),
            graph,
        }
    }

    pub fn graph(&self) -> &GraphClient {
        &self.graph
    }

    /// Whether the account has granted Graph access ("Sync with Outlook").
    /// In-memory after the first read, so it is safe on the GTK thread.
    pub fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Runs the combined interactive consent for Config's "Sync with
    /// Outlook" action.
    pub async fn ensure_outlook_consent(&self) -> Result<(), String> {
        self.oauth.ensure_graph_consent().await
    }

    /// Runs the interactive consent for one optional feature's scopes (rooms,
    /// shared calendars, the directory). Only ever called from a user action.
    pub async fn ensure_feature(&self, feature: GraphFeature) -> Result<(), String> {
        self.graph.ensure_feature(feature).await.map_err(|error| error.to_string())
    }
}

/// A Graph feature client built on a [`GraphAccount`]. Supplies the consent
/// gate every feature shares, so callers keep writing `client.consented()`.
pub trait GraphFeatureClient {
    fn account(&self) -> &GraphAccount;

    /// Whether the account has granted Graph access.
    fn consented(&self) -> bool {
        self.account().consented()
    }
}
