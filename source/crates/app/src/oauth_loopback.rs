//! The loopback half of the OAuth2 authorization-code flow (RFC 8252 §7.3),
//! shared by the Microsoft and Google sign-ins: a one-shot HTTP listener on
//! `127.0.0.1` that waits for the browser's redirect and pulls the
//! authorization code out of it.
//!
//! The redirect is only trusted when it carries the exact `state` the flow
//! generated. Anything else - a missing or wrong `state`, a favicon request, a
//! reload - is answered and ignored, so another local process (or a page that
//! makes the browser hit the port) can neither inject its own code nor cancel
//! the sign-in with a forged `error=`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::time::Duration;

use lookout_core::text::parse_query;

/// How long one connection may take to send its request line and headers. A
/// browser sends them at once; without a bound, a client that connects and
/// says nothing would hold the single accept loop until the flow times out.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What one request to the loopback listener amounts to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Redirect {
    /// The provider's redirect with the expected `state` and a code.
    Code(String),
    /// The provider's redirect with the expected `state` and an `error`.
    Denied(String),
    /// Anything else: answered with this message and ignored.
    Ignore(&'static str),
}

const NO_CODE: &str = "No authorization code received; close this window.";
const STATE_MISMATCH: &str = "Authorization state mismatch; close this window and try again.";

/// Classifies one HTTP request (its text up to the end of the headers)
/// against the `state` this flow sent. `state` is checked before anything
/// else is looked at, and a request without it is not trusted.
pub(crate) fn classify_redirect(request: &str, expected_state: &str) -> Redirect {
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let Some((_, query)) = path.split_once('?') else {
        return Redirect::Ignore(NO_CODE);
    };
    let params = parse_query(query);
    if params.get("state").map(String::as_str) != Some(expected_state) {
        return Redirect::Ignore(STATE_MISMATCH);
    }
    if let Some(error) = params.get("error") {
        return Redirect::Denied(error.clone());
    }
    match params.get("code") {
        Some(code) if !code.is_empty() => Redirect::Code(code.clone()),
        _ => Redirect::Ignore(NO_CODE),
    }
}

/// Serves the loopback redirect until the browser delivers the authorization
/// code, then tells the user to close the window. `provider` ("Microsoft",
/// "Google") names the sign-in in the cancelled/failed messages. The caller
/// bounds the whole wait with its own timeout.
pub(crate) async fn receive_auth_code(listener: tokio::net::TcpListener, state: &str, provider: &str) -> Result<String, String> {
    serve_redirect(listener, state, provider, REQUEST_READ_TIMEOUT).await
}

async fn serve_redirect(listener: tokio::net::TcpListener, state: &str, provider: &str, read_timeout: Duration) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let request = match tokio::time::timeout(read_timeout, read_request_until_headers(&mut sock)).await {
            Ok(Ok(request)) => request,
            Ok(Err(_)) | Err(_) => continue,
        };
        match classify_redirect(&request, state) {
            Redirect::Code(code) => {
                let _ = respond_html(&mut sock, "Authorization complete - you can close this window and return to Lookout.").await;
                return Ok(code);
            }
            Redirect::Denied(error) => {
                let _ = respond_html(&mut sock, &format!("{provider} sign-in was cancelled or failed.")).await;
                return Err(format!("{provider} authorization failed: {error}"));
            }
            Redirect::Ignore(message) => {
                let _ = respond_html(&mut sock, message).await;
            }
        }
    }
}

async fn read_request_until_headers(sock: &mut tokio::net::TcpStream) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = sock.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16384 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn respond_html(sock: &mut tokio::net::TcpStream, message: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head><body \
         style=\"font-family:sans-serif;margin:3rem;font-size:1.1rem\"><p>{message}</p></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    sock.write_all(resp.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n")
    }

    #[test]
    fn a_redirect_with_the_expected_state_yields_its_code() {
        assert_eq!(classify_redirect(&get("/?code=abc&state=S"), "S"), Redirect::Code("abc".to_string()));
    }

    #[test]
    fn a_code_without_the_expected_state_is_ignored() {
        assert_eq!(classify_redirect(&get("/?code=abc"), "S"), Redirect::Ignore(STATE_MISMATCH));
        assert_eq!(classify_redirect(&get("/?code=abc&state=X"), "S"), Redirect::Ignore(STATE_MISMATCH));
        assert_eq!(classify_redirect(&get("/?code=abc&state="), "S"), Redirect::Ignore(STATE_MISMATCH));
    }

    #[test]
    fn an_error_only_cancels_the_flow_with_the_expected_state() {
        assert_eq!(classify_redirect(&get("/?error=access_denied"), "S"), Redirect::Ignore(STATE_MISMATCH));
        assert_eq!(classify_redirect(&get("/?error=access_denied&state=S"), "S"), Redirect::Denied("access_denied".to_string()));
    }

    #[test]
    fn stray_requests_are_ignored() {
        assert_eq!(classify_redirect(&get("/favicon.ico"), "S"), Redirect::Ignore(NO_CODE));
        assert_eq!(classify_redirect(&get("/?state=S"), "S"), Redirect::Ignore(NO_CODE));
        assert_eq!(classify_redirect("", "S"), Redirect::Ignore(NO_CODE));
    }

    /// A connection that never sends its request is dropped after the read
    /// timeout instead of holding the accept loop, so the real redirect
    /// behind it still lands.
    #[tokio::test]
    async fn a_silent_connection_does_not_block_the_redirect() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move { serve_redirect(listener, "S", "Test", Duration::from_millis(50)).await });

        let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut browser = tokio::net::TcpStream::connect(addr).await.unwrap();
        browser.write_all(get("/?code=c&state=S").as_bytes()).await.unwrap();

        assert_eq!(receiver.await.unwrap(), Ok("c".to_string()));
        let mut reply = String::new();
        browser.read_to_string(&mut reply).await.unwrap();
        assert!(reply.contains("Authorization complete"));
    }

    #[tokio::test]
    async fn a_denied_redirect_names_the_provider() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move { receive_auth_code(listener, "S", "Microsoft").await });
        let mut browser = tokio::net::TcpStream::connect(addr).await.unwrap();
        browser.write_all(get("/?error=access_denied&state=S").as_bytes()).await.unwrap();
        assert_eq!(receiver.await.unwrap(), Err("Microsoft authorization failed: access_denied".to_string()));
    }
}
