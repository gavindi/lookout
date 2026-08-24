//! Config → Assistant: the OpenAI-compatible API endpoint the assistant
//! talks to. The base URL is a plain GSettings string
//! (`assistant-api-url`, see `settings.rs`); the API token never touches
//! dconf or `settings.json` - it lives in the GNOME keyring via the Secret
//! Service D-Bus API (`secret-service` crate, the same zbus stack as the
//! GOA and other-accounts integrations), one item addressed by stable
//! attributes so it can be found, replaced, and deleted without ever
//! reading it back for display.
//!
//! [`test_connection`] powers the Settings screen's "Test" button: a light
//! `GET {base}/models` probe with the Bearer token - the standard
//! OpenAI-compatible liveness check, answered by OpenAI, LM Studio, vLLM,
//! Ollama's `/v1` endpoint, and the rest. A 2xx status is a success; a
//! non-2xx answer or a transport failure is a user-facing error.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;

use secret_service::{EncryptionType, SecretService};

/// The keyring item attribute identifying Lookout's own items - the same
/// value as `other_accounts.rs`, so everything shows under one application
/// name in Seahorse.
const KEYRING_APPLICATION: &str = "io.github.gavindi.Lookout";

/// The `account` attribute value for the assistant's token slot, distinct
/// from mail accounts' `other:<uuid>` ids so the two can never collide in
/// the keyring.
const TOKEN_ACCOUNT: &str = "assistant";
const TOKEN_PROTOCOL: &str = "api-token";

fn token_attributes() -> HashMap<&'static str, &'static str> {
    let mut attrs = HashMap::new();
    attrs.insert("application", KEYRING_APPLICATION);
    attrs.insert("account", TOKEN_ACCOUNT);
    attrs.insert("protocol", TOKEN_PROTOCOL);
    attrs
}

/// The stored Assistant API token, if one has been configured. `None` means
/// "not configured yet" - a missing item is the normal first-run state and
/// is not an error; an `Err` is a keyring problem the user can act on.
pub async fn load_token() -> Result<Option<String>, String> {
    let service = SecretService::connect(EncryptionType::Dh).await.map_err(crate::other_accounts::keyring_error)?;
    let collection = service.get_default_collection().await.map_err(crate::other_accounts::keyring_error)?;
    let items = collection.search_items(token_attributes()).await.map_err(crate::other_accounts::keyring_error)?;
    let Some(item) = items.into_iter().next() else {
        return Ok(None);
    };
    let secret = item.get_secret().await.map_err(crate::other_accounts::keyring_error)?;
    match String::from_utf8(secret) {
        Ok(token) => Ok(Some(token)),
        Err(_) => Err("The stored Assistant API token is not valid text.".to_string()),
    }
}

/// Stores (or replaces) the Assistant API token. Best-effort unlock like
/// the account-passwords write: a locked default collection prompts the
/// user through the keyring daemon before the write lands.
pub async fn store_token(token: &str) -> Result<(), String> {
    let service = SecretService::connect(EncryptionType::Dh).await.map_err(crate::other_accounts::keyring_error)?;
    let collection = service.get_default_collection().await.map_err(crate::other_accounts::keyring_error)?;
    let _ = collection.unlock().await;
    collection
        .create_item("Lookout · Assistant API token", token_attributes(), token.as_bytes(), true, "text/plain")
        .await
        .map_err(crate::other_accounts::keyring_error)?;
    Ok(())
}

/// Removes the stored token - the "cleared the field" path. A missing item
/// is not an error.
pub async fn delete_token() -> Result<(), String> {
    let service = SecretService::connect(EncryptionType::Dh).await.map_err(crate::other_accounts::keyring_error)?;
    let collection = service.get_default_collection().await.map_err(crate::other_accounts::keyring_error)?;
    let items = collection.search_items(token_attributes()).await.map_err(crate::other_accounts::keyring_error)?;
    for item in items {
        item.delete().await.map_err(crate::other_accounts::keyring_error)?;
    }
    Ok(())
}

/// The Settings → Assistant "Test" button's probe: `GET {base}/models` with
/// the Bearer token. Success is a 2xx (reqwest follows the redirects some
/// hosts answer with); a non-2xx answer or a transport failure is an error
/// the caller surfaces in the row's subtitle. The models list itself is
/// deliberately never parsed - the probe only needs the status line - but a
/// short, sanitized error-body snippet rides along when the server answers
/// with a message, so a misconfigured host explains itself.
pub async fn test_connection(base_url: &str, token: &str) -> Result<(), String> {
    let url = models_url(base_url)?;
    let client = reqwest::Client::new();
    let mut request = client.get(&url).timeout(std::time::Duration::from_secs(10));
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) if e.is_builder() => return Err(format!("Invalid API URL {url:?}: {e}")),
        Err(e) => return Err(format!("Connection failed: {e}")),
    };
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    let mut snippet: String = body.chars().take(200).collect();
    if snippet.chars().count() < body.chars().count() {
        snippet.push('…');
    }
    let detail = if snippet.trim().is_empty() { String::new() } else { format!(" — {snippet}") };
    Err(format!("The server answered {status}{detail}"))
}

/// Fetches the API's available agents: the `id` of every model the
/// OpenAI-compatible `/models` endpoint reports (`data[].id`), sorted for
/// the dropdown. An empty list is a valid (if unusual) answer, so it is
/// not an error.
pub async fn list_models(base_url: &str, token: &str) -> Result<Vec<String>, String> {
    let url = models_url(base_url)?;
    let client = reqwest::Client::new();
    let mut request = client.get(&url).timeout(std::time::Duration::from_secs(10));
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) if e.is_builder() => return Err(format!("Invalid API URL {url:?}: {e}")),
        Err(e) => return Err(format!("Connection failed: {e}")),
    };
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let mut snippet: String = body.chars().take(200).collect();
        if snippet.chars().count() < body.chars().count() {
            snippet.push('…');
        }
        let detail = if snippet.trim().is_empty() { String::new() } else { format!(" — {snippet}") };
        return Err(format!("The server answered {status}{detail}"));
    }
    let body: serde_json::Value = match response.json().await {
        Ok(body) => body,
        Err(e) => return Err(format!("The server's answer wasn't a JSON /models list: {e}")),
    };
    let mut models: Vec<String> = body
        .get("data")
        .and_then(|data| data.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    Ok(models)
}

/// The `/models` endpoint URL for a base URL, tolerating whitespace and a
/// trailing slash. Empty input is rejected before any network I/O.
fn models_url(base_url: &str) -> Result<String, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Enter an API URL first".to_string());
    }
    Ok(format!("{base}/models"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(fut)
    }

    /// Serves one HTTP request against a path/status/body script and returns
    /// the request's request line, so tests can assert on what the probe
    /// actually sent. Listens on an ephemeral loopback port; `url_tail` is
    /// appended to the base URL so tests can exercise URL handling
    /// (e.g. a trailing slash).
    async fn serve_once(url_tail: &'static str, status: &'static str, body: &'static str) -> (Result<(), String>, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });

        let result = test_connection(&format!("http://127.0.0.1:{port}{url_tail}"), "sk-test").await;
        let request = server.await.unwrap();
        let request_line = request.lines().next().unwrap().to_string();
        (result, request_line)
    }

    #[test]
    fn successful_probe_hits_models_with_the_bearer_token() {
        let (result, request_line) = block_on(serve_once("", "200 OK", "{}"));
        assert!(result.is_ok(), "a 2xx must count as a successful connection: {result:?}");
        assert_eq!(request_line, "GET /models HTTP/1.1");
    }

    #[test]
    fn trailing_slash_is_tolerated() {
        let (result, request_line) = block_on(serve_once("/", "200 OK", "{}"));
        assert!(result.is_ok(), "a trailing slash must not break the probe: {result:?}");
        assert_eq!(request_line, "GET /models HTTP/1.1");
    }

    #[test]
    fn non_2xx_answers_are_errors_with_status_and_body_snippet() {
        let (result, request_line) = block_on(serve_once("", "401 Unauthorized", "{\"error\":\"bad key\"}"));
        let message = result.unwrap_err();
        assert!(message.contains("401"), "the status must be in the message: {message}");
        assert!(message.contains("bad key"), "a short body snippet must ride along: {message}");
        assert_eq!(request_line, "GET /models HTTP/1.1");
    }

    #[test]
    fn empty_or_blank_urls_are_rejected_before_any_network_io() {
        assert!(block_on(test_connection("", "sk-test")).unwrap_err().contains("URL"));
        assert!(block_on(test_connection("   ", "sk-test")).unwrap_err().contains("URL"));
        assert!(block_on(list_models("", "sk-test")).unwrap_err().contains("URL"));
    }

    /// Serves one `/models` request for `list_models`, returning the result
    /// and the request's request line.
    async fn serve_models_once(status: &'static str, body: &'static str) -> (Result<Vec<String>, String>, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes()).await;
            request
        });

        let result = list_models(&format!("http://127.0.0.1:{port}"), "sk-test").await;
        let request = server.await.unwrap();
        let request_line = request.lines().next().unwrap().to_string();
        (result, request_line)
    }

    #[test]
    fn model_list_parses_data_ids_sorted() {
        let (result, request_line) = block_on(serve_models_once("200 OK", r#"{"data":[{"id":"gpt-4o"},{"id":"llama-3.1"},{"id":"gpt-4o-mini"}]}"#));
        let models = result.unwrap();
        assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini", "llama-3.1"], "ids must be extracted and sorted");
        assert_eq!(request_line, "GET /models HTTP/1.1");
    }

    #[test]
    fn model_list_tolerates_a_missing_or_empty_data_array() {
        let (empty, _) = block_on(serve_models_once("200 OK", r#"{"object":"list","data":[]}"#));
        assert_eq!(empty.unwrap(), Vec::<String>::new());
        let (missing, _) = block_on(serve_models_once("200 OK", r#"{"object":"list"}"#));
        assert_eq!(missing.unwrap(), Vec::<String>::new());
        let (malformed, _) = block_on(serve_models_once("200 OK", "not json"));
        assert!(malformed.is_err(), "an unparseable body must be an error");
    }

    #[test]
    fn model_list_reports_non_2xx_with_status_and_snippet() {
        let (result, _) = block_on(serve_models_once("403 Forbidden", "{\"error\":\"no access\"}"));
        let message = result.unwrap_err();
        assert!(message.contains("403"), "the status must be in the message: {message}");
        assert!(message.contains("no access"), "a short body snippet must ride along: {message}");
    }
}