//! A minimal ManageSieve client (RFC 5804) - the transport half of the Sieve
//! rules backend (OUTLOOK_PARITY.md §4.5, roadmap item 2.5).
//!
//! Only what the rules backend needs is implemented: connect (implicit TLS,
//! STARTTLS or plain), read capabilities, `AUTHENTICATE "PLAIN"`, and upload
//! the generated script with `PUTSCRIPT` + `SETACTIVE`. The client is generic
//! over its stream so it can be driven against an in-memory stream in tests;
//! [`connect`] wires the real TLS/plain sockets.
//!
//! ManageSieve capability detection: the IMAP `SIEVE` capability is unreliable
//! across servers, so the backend also just tries the connection and degrades
//! when it fails (see `session.rs`).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::connection::{connect_plain, connect_tls, tls_connect_over, AsyncStream};
use crate::error::{Error, Result};

/// The default ManageSieve port (RFC 5804).
pub const DEFAULT_PORT: u16 = 4190;

/// How to reach the ManageSieve server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SieveSecurity {
    /// TLS from the first byte (port 4190 with a TLS listener).
    ImplicitTls,
    /// Plain TCP, upgraded with STARTTLS before authenticating.
    StartTls,
    /// Plain TCP with no encryption (localhost/testing only).
    Plain,
}

/// A server's advertised capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SieveCapabilities {
    pub implementation: Option<String>,
    /// The Sieve language extensions (from the `SIEVE` capability).
    pub extensions: Vec<String>,
    /// The SASL mechanisms (from the `SASL` capability).
    pub sasl_mechanisms: Vec<String>,
    pub starttls: bool,
    pub version: Option<String>,
}

impl SieveCapabilities {
    /// Whether the server offers a Sieve extension.
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|extension| extension.eq_ignore_ascii_case(name))
    }

    /// Whether the server offers PLAIN SASL auth.
    pub fn supports_plain(&self) -> bool {
        self.sasl_mechanisms.iter().any(|mechanism| mechanism.eq_ignore_ascii_case("PLAIN"))
    }
}

/// A ManageSieve status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    No,
    Bye,
}

/// The ManageSieve protocol driver over one stream. Generic so tests can hand
/// it a `tokio::io::duplex` half; [`ManageSieveClient`] is the boxed form the
/// application uses.
pub struct ManageSieve<S> {
    stream: S,
    /// Bytes read from the socket but not yet consumed as a line.
    buffer: Vec<u8>,
}

/// The boxed client the rest of the crate talks to.
pub type ManageSieveClient = ManageSieve<Box<dyn AsyncStream>>;

impl<S: AsyncRead + AsyncWrite + Unpin> ManageSieve<S> {
    pub fn new(stream: S) -> Self {
        ManageSieve { stream, buffer: Vec::new() }
    }

    /// Boxes the stream into the type-erased form used across the crate.
    pub fn boxed(self) -> ManageSieveClient
    where
        S: AsyncStream + 'static,
    {
        ManageSieve {
            stream: Box::new(self.stream),
            buffer: self.buffer,
        }
    }

    /// The stream back, for a STARTTLS upgrade. Any bytes already buffered are
    /// discarded (the server must not send anything between `OK` and the TLS
    /// handshake).
    fn into_inner(self) -> S {
        self.stream
    }

    /// Reads one CRLF-terminated line (without the terminator).
    async fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(position) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                let text = String::from_utf8_lossy(&line);
                return Ok(text.trim_end_matches(['\r', '\n']).to_string());
            }
            let mut chunk = [0u8; 1024];
            let read = self.stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(Error::Sieve("connection closed".to_string()));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Sends one command line (appending CRLF).
    async fn send_command(&mut self, line: &str) -> Result<()> {
        self.write_all(format!("{line}\r\n").as_bytes()).await
    }

    /// Reads lines until a status line, returning it (capability lines before
    /// it are discarded). The message text follows the status.
    async fn read_status(&mut self) -> Result<(Status, String)> {
        loop {
            let line = self.read_line().await?;
            if let Some((status, message)) = parse_status(&line) {
                return Ok((status, message));
            }
        }
    }

    /// Reads lines until a status line, requiring `OK`.
    async fn expect_ok(&mut self) -> Result<()> {
        let (status, message) = self.read_status().await?;
        match status {
            Status::Ok => Ok(()),
            _ => Err(Error::Sieve(message)),
        }
    }

    /// Reads the capability response (the greeting or a `CAPABILITY` reply):
    /// key/value lines until a status line, which must be `OK`.
    async fn read_capabilities(&mut self) -> Result<SieveCapabilities> {
        let mut capabilities = SieveCapabilities::default();
        loop {
            let line = self.read_line().await?;
            if let Some((status, message)) = parse_status(&line) {
                return match status {
                    Status::Ok => Ok(capabilities),
                    _ => Err(Error::Sieve(message)),
                };
            }
            apply_capability_line(&line, &mut capabilities);
        }
    }

    /// Reads the server greeting (capabilities + `OK`).
    pub async fn read_greeting(&mut self) -> Result<SieveCapabilities> {
        self.read_capabilities().await
    }

    /// Re-requests the capability list.
    pub async fn capabilities(&mut self) -> Result<SieveCapabilities> {
        self.send_command("CAPABILITY").await?;
        self.read_capabilities().await
    }

    /// Authenticates with SASL PLAIN.
    pub async fn authenticate_plain(&mut self, username: &str, password: &str) -> Result<()> {
        let raw = format!("\0{username}\0{password}");
        let encoded = BASE64.encode(raw.as_bytes());
        self.send_command(&format!("AUTHENTICATE \"PLAIN\" \"{encoded}\"")).await?;
        self.expect_ok().await.map_err(|_| Error::Sieve("ManageSieve authentication failed".to_string()))
    }

    /// Uploads `script` under `name`, replacing any existing script.
    pub async fn put_script(&mut self, name: &str, script: &str) -> Result<()> {
        let bytes = script.as_bytes();
        self.send_command(&format!("PUTSCRIPT \"{name}\" {{{}+}}", bytes.len())).await?;
        self.write_all(bytes).await?;
        self.write_all(b"\r\n").await?;
        self.expect_ok().await
    }

    /// Makes `name` the active script.
    pub async fn set_active(&mut self, name: &str) -> Result<()> {
        self.send_command(&format!("SETACTIVE \"{name}\"")).await?;
        self.expect_ok().await
    }

    /// Asks the server to syntax-check `script` without storing it.
    pub async fn check_script(&mut self, script: &str) -> Result<()> {
        let bytes = script.as_bytes();
        self.send_command(&format!("CHECKSCRIPT {{{}+}}", bytes.len())).await?;
        self.write_all(bytes).await?;
        self.write_all(b"\r\n").await?;
        self.expect_ok().await
    }

    /// Lists the account's scripts, with whether each is active.
    pub async fn list_scripts(&mut self) -> Result<Vec<(String, bool)>> {
        self.send_command("LISTSCRIPTS").await?;
        let mut scripts = Vec::new();
        loop {
            let line = self.read_line().await?;
            if let Some((status, message)) = parse_status(&line) {
                return match status {
                    Status::Ok => Ok(scripts),
                    _ => Err(Error::Sieve(message)),
                };
            }
            if let Some(name) = first_quoted(&line) {
                let active = line.to_ascii_uppercase().contains("ACTIVE");
                scripts.push((name, active));
            }
        }
    }

    /// Says goodbye (best effort).
    pub async fn logout(&mut self) -> Result<()> {
        self.send_command("LOGOUT").await?;
        let _ = self.read_status().await;
        Ok(())
    }
}

/// Parses a status line (`OK`, `NO`, `BYE`, each with optional text).
fn parse_status(line: &str) -> Option<(Status, String)> {
    let trimmed = line.trim_start();
    for (prefix, status) in [("OK", Status::Ok), ("NO", Status::No), ("BYE", Status::Bye)] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with(' ') {
                let message = extract_quote(rest).unwrap_or_else(|| rest.trim().to_string());
                return Some((status, message));
            }
        }
    }
    None
}

/// The first double-quoted token in a line.
fn first_quoted(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The text of the first quoted string, if any.
fn extract_quote(text: &str) -> Option<String> {
    first_quoted(text)
}

/// Applies one capability line to `capabilities`.
fn apply_capability_line(line: &str, capabilities: &mut SieveCapabilities) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    // A bare token (e.g. `STARTTLS`).
    if !line.starts_with('"') {
        if line.eq_ignore_ascii_case("STARTTLS") {
            capabilities.starttls = true;
        }
        return;
    }
    let tokens = quoted_tokens(line);
    let key = tokens.first().map(|token| token.to_ascii_uppercase()).unwrap_or_default();
    let value = tokens.get(1).cloned().unwrap_or_default();
    match key.as_str() {
        "IMPLEMENTATION" => capabilities.implementation = Some(value),
        "SIEVE" => capabilities.extensions = value.split_whitespace().map(str::to_string).collect(),
        "SASL" => capabilities.sasl_mechanisms = value.split_whitespace().map(str::to_string).collect(),
        "VERSION" => capabilities.version = Some(value),
        "STARTTLS" => capabilities.starttls = true,
        _ => {}
    }
}

/// Splits a line into its double-quoted tokens (un-escaping `\"` and `\\`).
fn quoted_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut token = String::new();
        while let Some(&ch) = chars.peek() {
            chars.next();
            match ch {
                '"' => break,
                '\\' => {
                    if let Some(&escaped) = chars.peek() {
                        chars.next();
                        token.push(escaped);
                    }
                }
                _ => token.push(ch),
            }
        }
        tokens.push(token);
    }
    tokens
}

/// Connects to a ManageSieve server and reads its greeting.
pub async fn connect(host: &str, port: u16, security: SieveSecurity) -> Result<ManageSieveClient> {
    match security {
        SieveSecurity::ImplicitTls => {
            let tls = connect_tls(host, port).await?;
            let mut client = ManageSieve::new(tls);
            client.read_greeting().await?;
            Ok(client.boxed())
        }
        SieveSecurity::Plain => {
            let tcp = connect_plain(host, port).await?;
            let mut client = ManageSieve::new(tcp);
            client.read_greeting().await?;
            Ok(client.boxed())
        }
        SieveSecurity::StartTls => {
            let tcp = connect_plain(host, port).await?;
            let mut client = ManageSieve::new(tcp);
            let capabilities = client.read_greeting().await?;
            if !capabilities.starttls {
                return Err(Error::Sieve("server does not offer STARTTLS".to_string()));
            }
            client.send_command("STARTTLS").await?;
            client.expect_ok().await?;
            let tcp = client.into_inner();
            let tls = tls_connect_over(tcp, host).await?;
            let mut client = ManageSieve::new(tls);
            // Per RFC 5804 the server issues a fresh capability response
            // immediately after a successful STARTTLS.
            client.read_greeting().await?;
            Ok(client.boxed())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    /// Reads one command line (CRLF-terminated) from the server half.
    async fn read_command(reader: &mut BufReader<DuplexStream>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line.trim_end_matches(['\r', '\n']).to_string()
    }

    #[test]
    fn parses_capability_lines() {
        let mut caps = SieveCapabilities::default();
        apply_capability_line("\"IMPLEMENTATION\" \"Dovecot Pigeonhole\"", &mut caps);
        apply_capability_line("\"SIEVE\" \"fileinto reject envelope imap4flags body copy\"", &mut caps);
        apply_capability_line("\"SASL\" \"PLAIN LOGIN\"", &mut caps);
        apply_capability_line("\"VERSION\" \"1.0\"", &mut caps);
        apply_capability_line("STARTTLS", &mut caps);
        assert_eq!(caps.implementation.as_deref(), Some("Dovecot Pigeonhole"));
        assert!(caps.has_extension("fileinto"));
        assert!(caps.has_extension("COPY"));
        assert!(caps.supports_plain());
        assert!(caps.starttls);
        assert_eq!(caps.version.as_deref(), Some("1.0"));
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status("OK \"done\"").unwrap(), (Status::Ok, "done".to_string()));
        assert_eq!(parse_status("NO \"bad script\"").unwrap(), (Status::No, "bad script".to_string()));
        assert_eq!(parse_status("OK").unwrap(), (Status::Ok, String::new()));
        assert!(parse_status("\"SIEVE\" \"fileinto\"").is_none());
    }

    #[test]
    fn first_quoted_extracts_a_name() {
        assert_eq!(first_quoted("\"lookout\" ACTIVE").as_deref(), Some("lookout"));
        assert_eq!(first_quoted("no quotes"), None);
    }

    #[tokio::test]
    async fn authenticates_and_uploads_a_script() {
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let script = "require [\"fileinto\"];\r\nkeep;\r\n";

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_stream);
            // Greeting.
            reader
                .get_mut()
                .write_all(b"\"IMPLEMENTATION\" \"Dovecot Pigeonhole\"\r\n\"SIEVE\" \"fileinto imap4flags\"\r\n\"SASL\" \"PLAIN\"\r\nOK \"ready\"\r\n")
                .await
                .unwrap();
            // AUTHENTICATE.
            let auth = read_command(&mut reader).await;
            assert!(auth.starts_with("AUTHENTICATE \"PLAIN\" \""));
            reader.get_mut().write_all(b"OK \"logged in\"\r\n").await.unwrap();
            // PUTSCRIPT with a literal.
            let put = read_command(&mut reader).await;
            assert_eq!(put, format!("PUTSCRIPT \"lookout\" {{{}+}}", script.len()));
            let mut body = vec![0u8; script.len()];
            tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body).await.unwrap();
            assert_eq!(String::from_utf8(body).unwrap(), script);
            let mut crlf = [0u8; 2];
            tokio::io::AsyncReadExt::read_exact(&mut reader, &mut crlf).await.unwrap();
            assert_eq!(&crlf, b"\r\n");
            reader.get_mut().write_all(b"OK \"stored\"\r\n").await.unwrap();
            // SETACTIVE.
            let active = read_command(&mut reader).await;
            assert_eq!(active, "SETACTIVE \"lookout\"");
            reader.get_mut().write_all(b"OK \"active\"\r\n").await.unwrap();
        });

        let mut client = ManageSieve::new(client_stream);
        let caps = client.read_greeting().await.unwrap();
        assert!(caps.has_extension("fileinto"));
        client.authenticate_plain("user", "pass").await.unwrap();
        client.put_script("lookout", script).await.unwrap();
        client.set_active("lookout").await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_no_status_becomes_an_error() {
        let (client_stream, server_stream) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_stream);
            reader.get_mut().write_all(b"\"SIEVE\" \"fileinto\"\r\nOK \"ready\"\r\n").await.unwrap();
            let _ = read_command(&mut reader).await;
            reader.get_mut().write_all(b"NO \"authentication failed\"\r\n").await.unwrap();
        });
        let mut client = ManageSieve::new(client_stream);
        client.read_greeting().await.unwrap();
        let result = client.authenticate_plain("user", "wrong").await;
        assert!(result.is_err());
    }
}
