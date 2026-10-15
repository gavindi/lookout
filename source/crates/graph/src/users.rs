//! Directory user resolution for shared mailboxes and delegated calendars
//! (OUTLOOK_PARITY.md item 5.6 / M-42, CAL-25).
//!
//! A shared mailbox or another user's calendar is addressed by SMTP address in
//! the UI but read through `/users/{id}/...` on Graph, so the address has to be
//! resolved to a user id once and cached. This module is the one place that
//! lookup lives, shared by the mail and calendar transports and the app.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license
use lookout_core::text::percent_encode;
use serde_json::Value;

use crate::client::GraphClient;
use crate::error::{GraphError, Result};

/// A directory user resolved from an address (`GET /users/{address}`): enough
/// to build `/users/{id}/...` paths and label the entry in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphUser {
    pub id: String,
    pub display_name: Option<String>,
    /// The user's SMTP address (`mail`), when Graph carries one.
    pub address: Option<String>,
}

/// Maps a Graph `user` object to a [`GraphUser`], or `None` when it has no id.
pub fn user_from_graph(value: &Value) -> Option<GraphUser> {
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    let display_name = value.get("displayName").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).map(str::to_string);
    let address = value
        .get("mail")
        .and_then(Value::as_str)
        .or_else(|| value.get("userPrincipalName").and_then(Value::as_str))
        .filter(|a| !a.trim().is_empty())
        .map(str::to_string);
    Some(GraphUser {
        id: id.to_string(),
        display_name,
        address,
    })
}

/// Resolves an address (or user id) to a [`GraphUser`] via `GET /users/{id}`,
/// or `None` when the directory has no such user.
pub async fn resolve_user(graph: &GraphClient, address: &str) -> Result<Option<GraphUser>> {
    let path = format!("users/{}?$select=id,displayName,mail,userPrincipalName", percent_encode(address));
    match graph.get(&path).await {
        Ok(value) => Ok(user_from_graph(&value)),
        Err(GraphError::Status { status: 404, .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use crate::error::Result as GraphResult;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> GraphResult<String> {
            Ok("stub-token".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> GraphResult<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> GraphClient {
        GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri()))
    }

    #[test]
    fn user_from_graph_reads_the_id_and_address() {
        let user = user_from_graph(&json!({
            "id": "u1",
            "displayName": "Support",
            "mail": "support@example.com",
            "userPrincipalName": "support@example.onmicrosoft.com"
        }))
        .unwrap();
        assert_eq!(user.id, "u1");
        assert_eq!(user.display_name.as_deref(), Some("Support"));
        assert_eq!(user.address.as_deref(), Some("support@example.com"));
    }

    #[test]
    fn user_from_graph_falls_back_to_the_upn_and_requires_an_id() {
        let user = user_from_graph(&json!({ "id": "u1", "userPrincipalName": "a@b.c" })).unwrap();
        assert_eq!(user.address.as_deref(), Some("a@b.c"));
        assert!(user_from_graph(&json!({ "displayName": "No id" })).is_none());
    }

    #[tokio::test]
    async fn resolve_user_percent_encodes_the_address_and_reads_the_user() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users/support%40example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1",
                "displayName": "Support",
                "mail": "support@example.com"
            })))
            .mount(&server)
            .await;

        let user = resolve_user(&client(&server), "support@example.com").await.unwrap().unwrap();
        assert_eq!(user.id, "u1");
    }

    #[tokio::test]
    async fn resolve_user_returns_none_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users/ghost%40example.com"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert!(resolve_user(&client(&server), "ghost@example.com").await.unwrap().is_none());
    }
}
