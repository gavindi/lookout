//! The Microsoft Graph mail-property transport: the HTTP operations the mail
//! sync drives, layered on [`GraphClient`] (OUTLOOK_PARITY.md §4.4, item 5.4 /
//! A-06, M-03, M-09, M-10, M-12).
//!
//! Every method returns `lookout_core` types through the pure mapping in the
//! parent module, so the sync glue never touches Graph JSON. `@odata.nextLink`
//! paging is handled by [`GraphClient::get_all_with_headers`], and the messages
//! delta by [`GraphClient::delta_with_headers`], whose cursor the caller
//! persists.
//!
//! Every request carries `Prefer: IdType="ImmutableId"`: a message's default
//! Graph id *changes* when it moves between folders, which would churn the
//! cached Message-ID ↔ Graph-id map and make a delta's removed/added pair
//! ambiguous. The immutable id is stable for the message's lifetime.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::text::percent_encode;
use lookout_core::{EmailAddress, FollowUpFlag, Importance};
use serde_json::{json, Value};

use crate::client::{page_items, GraphClient};
use crate::error::{GraphError, Result};

use super::{override_body, override_from_graph, InferenceClassification, InferenceOverride, MessageDelta, MessageProperties};

/// `Prefer: IdType="ImmutableId"` - see the module notes.
const IMMUTABLE_ID: [(&str, &str); 1] = [("Prefer", "IdType=\"ImmutableId\"")];

/// The `$select` fields the property read consumes - exactly what the mapper
/// needs, so Graph doesn't return whole message bodies.
const PROPERTY_SELECT: &str = "$select=id,internetMessageId,categories,flag,importance,inferenceClassification";

/// Most sender overrides read for one account. Graph itself caps a mailbox at
/// 1000 overrides.
const MAX_OVERRIDES: usize = 1000;

/// The mail-property HTTP operations, over a shared [`GraphClient`].
pub struct MailClient {
    graph: GraphClient,
}

impl MailClient {
    pub fn new(graph: GraphClient) -> Self {
        MailClient { graph }
    }

    /// Resolves an RFC 822 `Message-ID` (angle brackets included, as the IMAP
    /// envelope carries it) to Graph's message id, or `None` when no message
    /// matches yet (it may not have landed in Graph's index).
    pub async fn resolve_message_id(&self, message_id: &str) -> Result<Option<String>> {
        let filter = percent_encode(&super::message_id_filter(message_id));
        let path = format!("me/messages?$filter={filter}&$select=id");
        let page = self.graph.get_with_headers(&path, &IMMUTABLE_ID).await?;
        Ok(page_items(&page).first().and_then(|message| message.get("id")).and_then(Value::as_str).map(str::to_string))
    }

    /// Reads one message's synced properties by Graph id, or `None` when it no
    /// longer exists. `zone` interprets a follow-up flag's due date.
    pub async fn get_properties(&self, graph_id: &str, zone: Option<chrono_tz::Tz>) -> Result<Option<MessageProperties>> {
        let path = format!("me/messages/{}?{PROPERTY_SELECT}", encode_segment(graph_id));
        match self.graph.get_with_headers(&path, &IMMUTABLE_ID).await {
            Ok(value) => Ok(super::message_properties_from_graph(&value, zone)),
            Err(GraphError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One page (or incremental step) of the messages delta, split into live
    /// records and tombstones. Pass `None` for the initial read, or a
    /// previously returned [`MessageDelta::delta_link`] for the next one.
    pub async fn property_delta(&self, cursor: Option<&str>, zone: Option<chrono_tz::Tz>) -> Result<MessageDelta> {
        let path = format!("me/messages/delta?{PROPERTY_SELECT}&$top=100");
        let page = self.graph.delta_with_headers(&path, cursor, &IMMUTABLE_ID).await?;
        let mut delta = MessageDelta {
            delta_link: page.delta_link,
            ..Default::default()
        };
        for item in &page.items {
            if let Some(id) = super::removed_id(item) {
                delta.removals.push(id);
            } else if let Some(properties) = super::message_properties_from_graph(item, zone) {
                delta.upserts.push(properties);
            }
        }
        Ok(delta)
    }

    /// Applies a partial `PATCH` to one message (categories, importance,
    /// classification and/or the flag in one request), returning Graph's copy.
    pub async fn patch_message(&self, graph_id: &str, body: &Value) -> Result<Value> {
        self.graph
            .patch_with_headers(&format!("me/messages/{}", encode_segment(graph_id)), body, &IMMUTABLE_ID)
            .await
    }

    /// Replaces a message's categories with `categories` (empty clears them).
    pub async fn set_categories(&self, graph_id: &str, categories: &[String]) -> Result<()> {
        self.patch_message(graph_id, &super::categories_body(categories)).await.map(|_| ())
    }

    /// Sets a message's importance (M-12).
    pub async fn set_importance(&self, graph_id: &str, importance: Importance) -> Result<()> {
        self.patch_message(graph_id, &super::importance_body(importance)).await.map(|_| ())
    }

    /// Sets a message's Focused / Other classification (M-03).
    pub async fn set_inference_classification(&self, graph_id: &str, classification: InferenceClassification) -> Result<()> {
        self.patch_message(graph_id, &super::classification_body(classification)).await.map(|_| ())
    }

    /// Sets or clears a message's follow-up flag (M-10).
    pub async fn set_follow_up(&self, graph_id: &str, flag: &FollowUpFlag, zone: Option<chrono_tz::Tz>) -> Result<()> {
        self.patch_message(graph_id, &json!({ "flag": super::flag_body(flag, zone) })).await.map(|_| ())
    }

    /// The account's Focused / Other sender overrides (M-03).
    pub async fn list_overrides(&self) -> Result<Vec<InferenceOverride>> {
        let items = self
            .graph
            .get_all("me/inferenceClassification/overrides?$top=100&$select=id,classifyAs,senderEmailAddress", MAX_OVERRIDES)
            .await?;
        Ok(items.iter().filter_map(override_from_graph).collect())
    }

    /// Creates or updates a sender override (Graph upserts by SMTP address),
    /// returning Graph's copy with its id.
    pub async fn create_override(&self, classify_as: InferenceClassification, sender: &EmailAddress) -> Result<Option<InferenceOverride>> {
        let created = self.graph.post("me/inferenceClassification/overrides", &override_body(classify_as, sender)).await?;
        Ok(override_from_graph(&created))
    }

    /// Deletes a sender override by id ("stop moving").
    pub async fn delete_override(&self, override_id: &str) -> Result<()> {
        self.graph
            .delete(&format!("me/inferenceClassification/overrides/{}", encode_segment(override_id)))
            .await
            .map(|_| ())
    }
}

/// Percent-encodes a path segment (a Graph message/override id), leaving RFC
/// 3986 unreserved characters. Graph ids are base64-ish and may contain `+`,
/// `/` or `=`, which must not leak into the URL path.
fn encode_segment(segment: &str) -> String {
    percent_encode(segment)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use lookout_core::FlagStatus;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> Result<String> {
            Ok("stub".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> MailClient {
        MailClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())))
    }

    fn message_json() -> Value {
        json!({
            "id": "msg-1",
            "internetMessageId": "<abc@contoso.com>",
            "categories": ["Work"],
            "importance": "high",
            "inferenceClassification": "other"
        })
    }

    #[tokio::test]
    async fn resolve_message_id_sends_the_filter_and_immutable_id_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .and(query_param("$filter", "internetMessageId eq '<abc@contoso.com>'"))
            .and(query_param("$select", "id"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "msg-1" }] })))
            .mount(&server)
            .await;

        assert_eq!(client(&server).resolve_message_id("<abc@contoso.com>").await.unwrap().as_deref(), Some("msg-1"));
    }

    #[tokio::test]
    async fn resolve_message_id_is_none_when_graph_has_no_match() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;
        assert_eq!(client(&server).resolve_message_id("<missing@x>").await.unwrap(), None);
    }

    #[tokio::test]
    async fn get_properties_selects_the_fields_and_maps_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/msg-1"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
            .mount(&server)
            .await;

        let props = client(&server).get_properties("msg-1", None).await.unwrap().unwrap();
        assert_eq!(props.graph_id, "msg-1");
        assert_eq!(props.categories, vec!["Work".to_string()]);
        assert_eq!(props.importance, Importance::High);
        assert_eq!(props.classification, InferenceClassification::Other);
    }

    #[tokio::test]
    async fn get_properties_is_none_on_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(client(&server).get_properties("gone", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn property_delta_splits_upserts_from_removals_and_returns_the_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/delta"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [message_json(), { "id": "gone", "@removed": { "reason": "deleted" } }],
                "@odata.deltaLink": format!("{}/v1.0/delta-cursor", server.uri())
            })))
            .mount(&server)
            .await;

        let delta = client(&server).property_delta(None, None).await.unwrap();
        assert_eq!(delta.upserts.len(), 1);
        assert_eq!(delta.upserts[0].graph_id, "msg-1");
        assert_eq!(delta.removals, vec!["gone".to_string()]);
        assert_eq!(delta.delta_link.as_deref(), Some(format!("{}/v1.0/delta-cursor", server.uri()).as_str()));
    }

    #[tokio::test]
    async fn the_write_helpers_send_their_bodies() {
        let server = MockServer::start().await;
        for (suffix, body) in [
            ("msg-1", json!({ "categories": ["Work", "Home"] })),
            ("msg-2", json!({ "importance": "low" })),
            ("msg-3", json!({ "inferenceClassification": "focused" })),
        ] {
            Mock::given(method("PATCH"))
                .and(path(format!("/v1.0/me/messages/{suffix}")))
                .and(header("Prefer", "IdType=\"ImmutableId\""))
                .and(body_partial_json(body.clone()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": suffix })))
                .mount(&server)
                .await;
        }

        let client = client(&server);
        client.set_categories("msg-1", &["Work".to_string(), "Home".to_string()]).await.unwrap();
        client.set_importance("msg-2", Importance::Low).await.unwrap();
        client.set_inference_classification("msg-3", InferenceClassification::Focused).await.unwrap();
    }

    #[tokio::test]
    async fn set_follow_up_patches_the_flag_body() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/msg-1"))
            .and(body_partial_json(json!({ "flag": { "flagStatus": "complete" } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "msg-1" })))
            .mount(&server)
            .await;

        let flag = FollowUpFlag {
            status: FlagStatus::Complete,
            ..Default::default()
        };
        client(&server).set_follow_up("msg-1", &flag, None).await.unwrap();
    }

    #[tokio::test]
    async fn overrides_list_create_and_delete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/inferenceClassification/overrides"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "o1", "classifyAs": "focused", "senderEmailAddress": { "name": "Sam", "address": "sam@contoso.com" } }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/inferenceClassification/overrides"))
            .and(body_partial_json(json!({ "classifyAs": "other", "senderEmailAddress": { "address": "sam@contoso.com" } })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "o2", "classifyAs": "other", "senderEmailAddress": { "address": "sam@contoso.com" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/inferenceClassification/overrides/o1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = client(&server);
        let overrides = client.list_overrides().await.unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].classify_as, InferenceClassification::Focused);

        let sender = EmailAddress::new("sam@contoso.com");
        let created = client.create_override(InferenceClassification::Other, &sender).await.unwrap().unwrap();
        assert_eq!(created.id, "o2");

        client.delete_override("o1").await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_maps_to_a_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/msg-1"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        match client(&server).set_importance("msg-1", Importance::High).await.unwrap_err() {
            GraphError::Status { status, .. } => assert_eq!(status, 403),
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    #[test]
    fn encode_segment_escapes_path_specials() {
        assert_eq!(encode_segment("msg-1"), "msg-1");
        assert_eq!(encode_segment("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
