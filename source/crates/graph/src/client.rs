/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::auth::TokenProvider;
use crate::error::{GraphError, Result};
use crate::scopes::GraphFeature;

/// The Graph v1.0 service root every relative path is resolved against.
pub const GRAPH_BASE_URL: &str = "https://graph.microsoft.com/v1.0";

/// How many times a `429`/`503` response is retried before giving up.
const DEFAULT_MAX_RETRIES: u32 = 5;
/// Fallback wait when Graph throttles without a `Retry-After` header.
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Ceiling on any single retry wait.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(120);

/// One page of a `/delta` query: the collected items and the `@odata.deltaLink`
/// cursor to persist for the next incremental sync.
#[derive(Debug, Clone, Default)]
pub struct DeltaPage {
    pub items: Vec<Value>,
    pub delta_link: Option<String>,
}

/// A Microsoft Graph client over a [`TokenProvider`]. Cheap to clone-by-`Arc`
/// at call sites; holds one `reqwest::Client` (its connection pool) and the
/// token provider.
pub struct GraphClient {
    http: reqwest::Client,
    base_url: String,
    tokens: Arc<dyn TokenProvider>,
    max_retries: u32,
}

impl GraphClient {
    /// A client against the public Graph v1.0 endpoint.
    pub fn new(tokens: Arc<dyn TokenProvider>) -> Self {
        Self::with_base_url(tokens, GRAPH_BASE_URL)
    }

    /// A client against an explicit service root (tests, sovereign clouds).
    pub fn with_base_url(tokens: Arc<dyn TokenProvider>, base_url: impl Into<String>) -> Self {
        GraphClient {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            tokens,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// The service root this client resolves relative paths against.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Negotiates consent for a feature's scopes before its first request. A
    /// no-op when the user has already granted them.
    pub async fn ensure_feature(&self, feature: GraphFeature) -> Result<()> {
        self.tokens.ensure_scopes(&feature.consent_scopes()).await
    }

    /// A single `GET` returning the parsed JSON body.
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.send_json(Method::GET, path, None).await
    }

    /// A `PATCH` with a JSON body, returning the parsed response body (Graph
    /// returns the updated resource, or an empty body for some endpoints,
    /// which surfaces as `Value::Null`).
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send_json(Method::PATCH, path, Some(body)).await
    }

    /// A `POST` with a JSON body.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send_json(Method::POST, path, Some(body)).await
    }

    /// Follows `@odata.nextLink` until the collection ends or `max_items` is
    /// reached, flattening every page's `value` array. This is the shape every
    /// list endpoint (`/me/messages`, `/me/contacts`, …) uses.
    pub async fn get_all(&self, path: &str, max_items: usize) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut next = Some(self.absolute(path));
        while let Some(url) = next {
            let page = self.get(&url).await?;
            for value in page_items(&page) {
                items.push(value.clone());
                if items.len() >= max_items {
                    return Ok(items);
                }
            }
            next = next_link(&page).map(str::to_string);
        }
        Ok(items)
    }

    /// Reads one `/delta` result: pass `None` (or a path) for the initial full
    /// read, or a previously returned [`DeltaPage::delta_link`] for an
    /// incremental one. Follows any `@odata.nextLink` pages internally and
    /// returns the fresh delta cursor.
    pub async fn delta(&self, path: &str, delta_link: Option<&str>) -> Result<DeltaPage> {
        let mut items = Vec::new();
        let mut next = Some(delta_link.map(str::to_string).unwrap_or_else(|| self.absolute(path)));
        let mut new_delta_link = None;
        while let Some(url) = next {
            let page = self.get(&url).await?;
            items.extend(page_items(&page).iter().cloned());
            if let Some(link) = delta_link_of(&page) {
                new_delta_link = Some(link.to_string());
                next = None;
            } else {
                next = next_link(&page).map(str::to_string);
            }
        }
        Ok(DeltaPage {
            items,
            delta_link: new_delta_link,
        })
    }

    /// The token-authenticated request loop: sends the request, waits out
    /// throttling, and maps the response into [`GraphError`].
    async fn send_json(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = self.absolute(path);
        let mut attempt = 0u32;
        loop {
            let token = self.tokens.graph_token().await?;
            let mut request = self.http.request(method.clone(), &url).bearer_auth(token);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await?;
            let status = response.status();

            if status.is_success() {
                let bytes = response.bytes().await?;
                if bytes.is_empty() {
                    return Ok(Value::Null);
                }
                return Ok(serde_json::from_slice(&bytes)?);
            }

            if is_throttled(status) {
                let retry_after = retry_after(response.headers());
                if attempt < self.max_retries {
                    attempt += 1;
                    let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                    tracing::debug!(%url, attempt, ?delay, "Graph throttled; retrying");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(GraphError::Throttled {
                    attempts: attempt + 1,
                    retry_after,
                });
            }

            return Err(Self::error_for(status, response).await);
        }
    }

    async fn error_for(status: StatusCode, response: reqwest::Response) -> GraphError {
        if status == StatusCode::UNAUTHORIZED {
            return GraphError::Unauthorized;
        }
        let request_id = response.headers().get("request-id").and_then(|value| value.to_str().ok()).map(str::to_string);
        let message = response.text().await.unwrap_or_default();
        GraphError::Status {
            status: status.as_u16(),
            message: truncate(message, 600),
            request_id,
        }
    }

    /// Resolves a path or an absolute URL (a page/`delta` link Graph returned)
    /// against the service root.
    fn absolute(&self, url: &str) -> String {
        if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("{}/{}", self.base_url, url.trim_start_matches('/'))
        }
    }
}

/// The `value` array of a Graph collection response (empty for non-collection
/// or delta-terminator pages).
pub fn page_items(page: &Value) -> &[Value] {
    page.get("value").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// The `@odata.nextLink` of a page, if it has another page.
pub fn next_link(page: &Value) -> Option<&str> {
    page.get("@odata.nextLink").and_then(Value::as_str)
}

/// The `@odata.deltaLink` of a page, if this is the last page of a delta read.
pub fn delta_link_of(page: &Value) -> Option<&str> {
    page.get("@odata.deltaLink").and_then(Value::as_str)
}

/// The `Retry-After` header in whole seconds. Graph sends a numeric value on
/// `429`; HTTP-date forms are ignored (retry falls back to the backoff).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn is_throttled(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE
}

fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    (DEFAULT_RETRY_DELAY * 2u32.pow(shift)).min(MAX_RETRY_DELAY)
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        // Truncate on a UTF-8 char boundary so a multi-byte character at the
        // cut point can't panic.
        let end = (0..=max).rev().find(|&index| text.is_char_boundary(index)).unwrap_or(0);
        text.truncate(end);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::error::GraphError;

    /// A token provider that always returns a fixed token, so the transport can
    /// be exercised without OAuth.
    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> Result<String> {
            Ok("stub-token".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> GraphClient {
        GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri()))
    }

    #[test]
    fn page_helpers_read_the_odata_annotations() {
        let page = json!({
            "value": [{"id": "1"}],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/messages?page=2"
        });
        assert_eq!(page_items(&page).len(), 1);
        assert_eq!(next_link(&page), Some("https://graph.microsoft.com/v1.0/me/messages?page=2"));
        assert_eq!(delta_link_of(&page), None);

        let delta = json!({ "value": [], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=x" });
        assert!(page_items(&delta).is_empty());
        assert_eq!(delta_link_of(&delta), Some("https://graph.microsoft.com/v1.0/me/messages/delta?$deltatoken=x"));
    }

    #[test]
    fn absolute_urls_pass_through_and_paths_resolve_against_the_base() {
        let client = GraphClient::with_base_url(Arc::new(StubTokens), "https://graph.microsoft.com/v1.0/");
        assert_eq!(client.base_url(), "https://graph.microsoft.com/v1.0");
        assert_eq!(client.absolute("me/messages"), "https://graph.microsoft.com/v1.0/me/messages");
        assert_eq!(client.absolute("/me/messages"), "https://graph.microsoft.com/v1.0/me/messages");
        assert_eq!(client.absolute("https://graph.microsoft.com/beta/me"), "https://graph.microsoft.com/beta/me");
    }

    #[tokio::test]
    async fn get_all_follows_next_link_until_the_collection_ends() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "1"}],
                "@odata.nextLink": format!("{}/v1.0/messages-page-2", server.uri())
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/messages-page-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{"id": "2"}] })))
            .mount(&server)
            .await;

        let items = client(&server).get_all("me/messages", 100).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], Value::String("1".into()));
        assert_eq!(items[1]["id"], Value::String("2".into()));
    }

    #[tokio::test]
    async fn get_all_stops_at_max_items() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "1"}, {"id": "2"}, {"id": "3"}]
            })))
            .mount(&server)
            .await;
        let items = client(&server).get_all("me/messages", 2).await.unwrap();
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn delta_returns_the_cursor_for_the_next_sync() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "1"}],
                "@odata.deltaLink": format!("{}/v1.0/delta-cursor", server.uri())
            })))
            .mount(&server)
            .await;

        let page = client(&server).delta("me/messages/delta", None).await.unwrap();
        assert_eq!(page.items.len(), 1);
        let expected = format!("{}/v1.0/delta-cursor", server.uri());
        assert_eq!(page.delta_link.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test]
    async fn throttling_is_retried_after_the_retry_after_delay() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{"id": "1"}] })))
            .mount(&server)
            .await;

        let items = client(&server).get_all("me/messages", 10).await.unwrap();
        assert_eq!(items.len(), 1, "the request succeeded after the retry");
    }

    #[tokio::test]
    async fn a_persistent_throttle_gives_up_with_throttled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .mount(&server)
            .await;

        let error = client(&server).with_max_retries(2).get("me/messages").await.unwrap_err();
        assert!(matches!(error, GraphError::Throttled { attempts: 3, .. }), "got {error:?}");
    }

    #[tokio::test]
    async fn unauthorized_and_other_statuses_map_to_their_variants() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        assert!(matches!(client(&server).get("me/messages").await.unwrap_err(), GraphError::Unauthorized));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden").insert_header("request-id", "req-1"))
            .mount(&server)
            .await;
        match client(&server).get("me/messages").await.unwrap_err() {
            GraphError::Status { status, request_id, .. } => {
                assert_eq!(status, 403);
                assert_eq!(request_id.as_deref(), Some("req-1"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn patch_sends_the_body_and_reads_the_response() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "abc", "importance": "high" })))
            .mount(&server)
            .await;
        let response = client(&server).patch("me/messages/abc", &json!({ "importance": "high" })).await.unwrap();
        assert_eq!(response["importance"], Value::String("high".into()));
    }
}
