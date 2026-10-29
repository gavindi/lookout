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

/// The HTTP client every [`GraphClient`] shares, so the app's many Graph
/// clients (one per feature and account) use one connection pool and reuse
/// TLS sessions instead of each handshaking with Graph on its own.
#[cfg(not(test))]
static HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(reqwest::Client::new);

/// A handle on [`HTTP`]; cloning only bumps a reference count. The app runs
/// one tokio runtime, so pooled connections never outlive theirs. Tests build
/// a runtime each, so they get a fresh client to keep a dead runtime's
/// connection out of the next test.
#[cfg(not(test))]
fn shared_http() -> reqwest::Client {
    HTTP.clone()
}

#[cfg(test)]
fn shared_http() -> reqwest::Client {
    reqwest::Client::new()
}

/// One page of a `/delta` query: the collected items and the `@odata.deltaLink`
/// cursor to persist for the next incremental sync.
#[derive(Debug, Clone, Default)]
pub struct DeltaPage {
    pub items: Vec<Value>,
    pub delta_link: Option<String>,
}

/// A request body: JSON for the ordinary endpoints, or raw bytes with their
/// own `Content-Type` for Graph's MIME `sendMail`.
enum RequestBody<'a> {
    Json(&'a Value),
    Raw { bytes: &'a [u8], content_type: &'a str },
}

/// A Microsoft Graph client over a [`TokenProvider`]. Cheap to clone-by-`Arc`
/// at call sites; holds a handle on the shared `reqwest::Client` (one
/// connection pool for every Graph client) and the token provider.
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
            http: shared_http(),
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
        self.send_json(Method::GET, path, None, &[]).await
    }

    /// A `GET` carrying extra request headers (e.g. `ConsistencyLevel:
    /// eventual` for a `/users?$search=` directory query, which Graph rejects
    /// without it), returning the parsed JSON body.
    pub async fn get_with_headers(&self, path: &str, headers: &[(&str, &str)]) -> Result<Value> {
        self.send_json(Method::GET, path, None, headers).await
    }

    /// A `GET` whose body is binary rather than JSON (`/photo/$value`),
    /// returning the raw bytes, or `None` when the resource carries no binary
    /// content (Graph answers HTTP 404 for a contact/user with no photo). A
    /// thin wrapper over [`Self::get_bytes_with_headers`] with no extra headers.
    pub async fn get_bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.get_bytes_with_headers(path, &[]).await
    }

    /// [`Self::get_bytes`] carrying extra request headers - the mail transport's
    /// `Prefer: IdType="ImmutableId"` on a message's `/$value` MIME read
    /// (MAIL_GRAPH_OVERVIEW.md item 5.7.1), so the id in the path is the stable
    /// immutable id rather than one that changes on a folder move.
    pub async fn get_bytes_with_headers(&self, path: &str, headers: &[(&str, &str)]) -> Result<Option<Vec<u8>>> {
        match self.send_response(Method::GET, path, None, headers).await {
            Ok(response) => Ok(Some(response.bytes().await?.to_vec())),
            Err(GraphError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A `PATCH` with a JSON body, returning the parsed response body (Graph
    /// returns the updated resource, or an empty body for some endpoints,
    /// which surfaces as `Value::Null`).
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send_json(Method::PATCH, path, Some(body), &[]).await
    }

    /// A `PATCH` carrying extra request headers (`If-Match` against a
    /// `changeKey`, `Prefer: return=representation`, …), returning the parsed
    /// response body. The calendar backend's event updates use it.
    pub async fn patch_with_headers(&self, path: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
        self.send_json(Method::PATCH, path, Some(body), headers).await
    }

    /// A `POST` with a JSON body.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send_json(Method::POST, path, Some(body), &[]).await
    }

    /// A `POST` carrying extra request headers - the mail transport's
    /// `Prefer: IdType="ImmutableId"` on a move, copy or new draft, so the id
    /// Graph returns is the stable one.
    pub async fn post_with_headers(&self, path: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
        self.send_json(Method::POST, path, Some(body), headers).await
    }

    /// A bodiless `POST` carrying extra request headers, for actions Graph
    /// takes no request body for (sending a draft).
    pub async fn post_empty_with_headers(&self, path: &str, headers: &[(&str, &str)]) -> Result<Value> {
        self.send_json(Method::POST, path, None, headers).await
    }

    /// A `DELETE` (Graph returns `204 No Content`; the body surfaces as
    /// `Value::Null`). Used to remove a `masterCategory` (item 2.1).
    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.send_json(Method::DELETE, path, None, &[]).await
    }

    /// A `DELETE` carrying extra request headers (e.g. `If-Match` against a
    /// `changeKey`).
    pub async fn delete_with_headers(&self, path: &str, headers: &[(&str, &str)]) -> Result<Value> {
        self.send_json(Method::DELETE, path, None, headers).await
    }

    /// Follows `@odata.nextLink` until the collection ends or `max_items` is
    /// reached, flattening every page's `value` array. This is the shape every
    /// list endpoint (`/me/messages`, `/me/contacts`, …) uses. A thin wrapper
    /// over [`Self::get_all_with_headers`] with no extra headers.
    pub async fn get_all(&self, path: &str, max_items: usize) -> Result<Vec<Value>> {
        self.get_all_with_headers(path, max_items, &[]).await
    }

    /// [`Self::get_all`] carrying extra request headers on every page - the
    /// mail-property sync's `Prefer: IdType="ImmutableId"` (item 5.4), which
    /// keeps a message's Graph id stable across folder moves.
    pub async fn get_all_with_headers(&self, path: &str, max_items: usize, headers: &[(&str, &str)]) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut next = Some(self.absolute(path));
        while let Some(url) = next {
            let page = self.get_with_headers(&url, headers).await?;
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
    /// returns the fresh delta cursor. A thin wrapper over
    /// [`Self::delta_with_headers`] with no extra headers.
    pub async fn delta(&self, path: &str, delta_link: Option<&str>) -> Result<DeltaPage> {
        self.delta_with_headers(path, delta_link, &[]).await
    }

    /// [`Self::delta`] carrying extra request headers on every page - the
    /// mail-property sync's `Prefer: IdType="ImmutableId"` (item 5.4).
    pub async fn delta_with_headers(&self, path: &str, delta_link: Option<&str>, headers: &[(&str, &str)]) -> Result<DeltaPage> {
        let mut items = Vec::new();
        let mut next = Some(delta_link.map(str::to_string).unwrap_or_else(|| self.absolute(path)));
        let mut new_delta_link = None;
        while let Some(url) = next {
            let page = self.get_with_headers(&url, headers).await?;
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

    /// Decodes a successful response into a JSON body (or `Value::Null` for an
    /// empty one, e.g. a `204 No Content`).
    async fn send_json(&self, method: Method, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<Value> {
        let response = self.send_response(method, path, body, headers).await?;
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// A `POST` of a raw request body with an explicit `Content-Type`, for the
    /// endpoints that take bytes rather than JSON - Graph's MIME `sendMail`
    /// (`Content-Type: text/rfc822`), which the local outbox drains through so
    /// it sends the exact RFC 5322 message the mail crate built (attachments,
    /// iMIP payloads and read receipts included). Returns the parsed response
    /// or `Value::Null` for an empty `202`.
    pub async fn post_raw(&self, path: &str, bytes: &[u8], content_type: &str) -> Result<Value> {
        let response = self.send_request(Method::POST, path, Some(RequestBody::Raw { bytes, content_type }), &[]).await?;
        let body = response.bytes().await?;
        if body.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&body)?)
    }

    /// The token-authenticated request loop: sends the request, waits out
    /// throttling, and returns the successful response for the caller to decode
    /// (JSON for [`Self::send_json`], raw bytes for [`Self::get_bytes`]).
    async fn send_response(&self, method: Method, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<reqwest::Response> {
        self.send_request(method, path, body.map(RequestBody::Json), headers).await
    }

    async fn send_request(&self, method: Method, path: &str, body: Option<RequestBody<'_>>, headers: &[(&str, &str)]) -> Result<reqwest::Response> {
        let url = self.checked_url(path)?;
        let mut attempt = 0u32;
        // A 401 means the cached access token was rejected (expired while the
        // machine slept, revoked, or issued for the wrong audience). Evict it
        // and retry exactly once with a freshly refreshed token; a second 401
        // is a real authorization failure. Kept separate from the throttle
        // counter so one does not consume the other's budget.
        let mut reauthorized = false;
        loop {
            let token = self.tokens.graph_token().await?;
            let mut request = self.http.request(method.clone(), url.clone()).bearer_auth(token);
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            if let Some(body) = body.as_ref() {
                request = match body {
                    RequestBody::Json(value) => request.json(value),
                    RequestBody::Raw { bytes, content_type } => request.header(reqwest::header::CONTENT_TYPE, *content_type).body(bytes.to_vec()),
                };
            }
            let response = request.send().await?;
            let status = response.status();

            if status.is_success() {
                return Ok(response);
            }

            if status == StatusCode::UNAUTHORIZED && !reauthorized {
                reauthorized = true;
                self.tokens.invalidate().await;
                continue;
            }

            if is_throttled(status) {
                let retry_after = retry_after(response.headers());
                // `Retry-After` is a server hint, not a command: never block
                // this (serial) session actor for longer than the cap. When
                // the server asks for more than we will wait, give up now and
                // let the caller reschedule instead of sleeping inside the
                // actor - a 1-hour `Retry-After` used to freeze every queued
                // move, send and body fetch behind it.
                if retry_after.is_some_and(|delay| delay > MAX_RETRY_DELAY) {
                    return Err(GraphError::Throttled {
                        attempts: attempt + 1,
                        retry_after,
                    });
                }
                if attempt < self.max_retries {
                    attempt += 1;
                    let delay = retry_after.unwrap_or_else(|| backoff(attempt)).min(MAX_RETRY_DELAY);
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

    /// [`Self::absolute`], refusing any URL whose origin differs from the
    /// service root's: the result gets the bearer token, and an absolute URL
    /// can come from a server response or the on-disk cache. The path may
    /// differ (`/beta` on the same host is fine).
    fn checked_url(&self, path: &str) -> Result<reqwest::Url> {
        let resolved = self.absolute(path);
        let untrusted = || GraphError::UntrustedUrl(truncate(resolved.clone(), 200));
        let url = reqwest::Url::parse(&resolved).map_err(|_| untrusted())?;
        let base = reqwest::Url::parse(&self.base_url).map_err(|_| untrusted())?;
        if same_origin(&url, &base) {
            Ok(url)
        } else {
            Err(untrusted())
        }
    }
}

/// Whether two URLs share scheme, host and (effective) port.
fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme() && a.host_str().is_some() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{header, method, path};
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

    /// A provider whose cached token starts out rejected: `graph_token` hands
    /// out `stale-token` until `invalidate` is called, then `fresh-token`.
    /// Counts invalidations so a test can pin "exactly once".
    struct ReauthTokens {
        invalidations: AtomicUsize,
    }

    #[async_trait]
    impl TokenProvider for ReauthTokens {
        async fn graph_token(&self) -> Result<String> {
            let token = if self.invalidations.load(Ordering::SeqCst) == 0 { "stale-token" } else { "fresh-token" };
            Ok(token.to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<()> {
            Ok(())
        }
        async fn invalidate(&self) {
            self.invalidations.fetch_add(1, Ordering::SeqCst);
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

    #[test]
    fn the_token_only_goes_to_the_service_roots_origin() {
        let client = GraphClient::with_base_url(Arc::new(StubTokens), "https://graph.microsoft.com/v1.0");
        for allowed in [
            "me/messages",
            "https://graph.microsoft.com/v1.0/me/messages?$skiptoken=x",
            "https://graph.microsoft.com/beta/me",
            "https://graph.microsoft.com:443/v1.0/me",
            "https://GRAPH.microsoft.com/v1.0/me",
        ] {
            assert!(client.checked_url(allowed).is_ok(), "{allowed} should be allowed");
        }
        for refused in [
            "https://evil.example/v1.0/me",
            "http://graph.microsoft.com/v1.0/me",
            "https://graph.microsoft.com@evil.example/v1.0/me",
            "https://graph.microsoft.com.evil.example/v1.0/me",
            "https://graph.microsoft.com:8443/v1.0/me",
        ] {
            assert!(matches!(client.checked_url(refused), Err(GraphError::UntrustedUrl(_))), "{refused} should be refused");
        }
    }

    #[tokio::test]
    async fn a_next_link_to_another_host_is_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "1"}],
                "@odata.nextLink": "https://evil.example/v1.0/messages-page-2"
            })))
            .mount(&server)
            .await;

        let result = client(&server).get_all("me/messages", 100).await;
        assert!(matches!(result, Err(GraphError::UntrustedUrl(ref url)) if url.contains("evil.example")), "got {result:?}");
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

    /// A `Retry-After` longer than the cap must not be slept inside the
    /// session actor: the request gives up immediately so the caller can
    /// reschedule.
    #[tokio::test]
    async fn an_over_cap_retry_after_gives_up_without_sleeping() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "3600"))
            .mount(&server)
            .await;

        let started = Instant::now();
        let error = client(&server).get("me/messages").await.unwrap_err();
        assert!(
            matches!(error, GraphError::Throttled { attempts: 1, retry_after: Some(delay) } if delay == Duration::from_secs(3600)),
            "got {error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2), "the client slept instead of rescheduling");
    }

    /// A 401 evicts the cached access token and retries exactly once with the
    /// refreshed one.
    #[tokio::test]
    async fn a_401_evicts_the_token_and_retries_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .and(header("Authorization", "Bearer stale-token"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .and(header("Authorization", "Bearer fresh-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;

        let tokens = Arc::new(ReauthTokens {
            invalidations: AtomicUsize::new(0),
        });
        let client = GraphClient::with_base_url(tokens.clone(), format!("{}/v1.0", server.uri()));
        client.get("me/messages").await.unwrap();
        assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1, "the stale token is evicted once");
    }

    /// A second 401 is a real authorization failure, not another refresh.
    #[tokio::test]
    async fn a_second_401_is_reported_as_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let tokens = Arc::new(ReauthTokens {
            invalidations: AtomicUsize::new(0),
        });
        let client = GraphClient::with_base_url(tokens.clone(), format!("{}/v1.0", server.uri()));
        assert!(matches!(client.get("me/messages").await.unwrap_err(), GraphError::Unauthorized));
        assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1, "only the first 401 triggers a refresh");
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

    #[tokio::test]
    async fn get_with_headers_sends_the_extra_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users"))
            .and(header("ConsistencyLevel", "eventual"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "u1" }] })))
            .mount(&server)
            .await;

        let page = client(&server).get_with_headers("users", &[("ConsistencyLevel", "eventual")]).await.unwrap();
        assert_eq!(page_items(&page).len(), 1);
    }

    #[tokio::test]
    async fn get_all_with_headers_sends_the_extra_header_on_every_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "1"}],
                "@odata.nextLink": format!("{}/v1.0/messages-page-2", server.uri())
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/messages-page-2"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{"id": "2"}] })))
            .mount(&server)
            .await;

        let items = client(&server)
            .get_all_with_headers("me/messages", 100, &[("Prefer", "IdType=\"ImmutableId\"")])
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn delta_with_headers_sends_the_extra_header_and_returns_the_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/delta"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{"id": "1"}],
                "@odata.deltaLink": format!("{}/v1.0/delta-cursor", server.uri())
            })))
            .mount(&server)
            .await;

        let page = client(&server)
            .delta_with_headers("me/messages/delta", None, &[("Prefer", "IdType=\"ImmutableId\"")])
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.delta_link.as_deref(), Some(format!("{}/v1.0/delta-cursor", server.uri()).as_str()));
    }

    #[tokio::test]
    async fn get_bytes_returns_the_body_or_none_on_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/contacts/c1/photo/$value"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8, 2, 3]))
            .mount(&server)
            .await;

        let bytes = client(&server).get_bytes("me/contacts/c1/photo/$value").await.unwrap();
        assert_eq!(bytes.as_deref(), Some([1u8, 2, 3].as_slice()));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/contacts/c2/photo/$value"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(client(&server).get_bytes("me/contacts/c2/photo/$value").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_bytes_with_headers_sends_the_extra_header_and_reads_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/msg-1/$value"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"Message-ID: <a@b>\r\n".to_vec()))
            .mount(&server)
            .await;

        let bytes = client(&server)
            .get_bytes_with_headers("me/messages/msg-1/$value", &[("Prefer", "IdType=\"ImmutableId\"")])
            .await
            .unwrap();
        assert_eq!(bytes.as_deref(), Some(b"Message-ID: <a@b>\r\n".as_slice()));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/gone/$value"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(client(&server)
            .get_bytes_with_headers("me/messages/gone/$value", &[("Prefer", "IdType=\"ImmutableId\"")])
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn post_raw_sends_the_bytes_and_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .and(header("Content-Type", "text/rfc822"))
            .and(wiremock::matchers::body_string("Subject: hi\r\n\r\nbody"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let response = client(&server).post_raw("me/sendMail", b"Subject: hi\r\n\r\nbody", "text/rfc822").await.unwrap();
        assert_eq!(response, Value::Null);
    }

    #[tokio::test]
    async fn delete_accepts_an_empty_no_content_response() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/outlook/masterCategories/cat-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let response = client(&server).delete("me/outlook/masterCategories/cat-1").await.unwrap();
        assert_eq!(response, Value::Null);
    }
}
