//! Lookout-managed OAuth2 for Microsoft 365 accounts.
//!
//! GOA's Microsoft 365 provider (`ms_graph`) requests only Microsoft Graph
//! scopes (`mail.readwrite`, `user.read`, ...), so the token it hands out is
//! never usable for Exchange Online IMAP/SMTP - those endpoints only accept
//! tokens carrying `https://outlook.office.com/IMAP.AccessAsUser.All` /
//! `https://outlook.office.com/SMTP.Send` (verified live against
//! outlook.office365.com: a Graph-scoped token gets `NO AUTHENTICATE
//! failed`). Microsoft accounts therefore bypass GOA for credentials and use
//! Lookout's own public-client authorization-code flow with PKCE and a
//! loopback redirect - the same shape Thunderbird's Microsoft provider uses.
//!
//! The refresh token is persisted per account under
//! `$XDG_DATA_HOME/lookout/oauth/<account>.json` with 0600 permissions, so
//! only the first connect needs the interactive browser step; every later
//! connect refreshes the access token silently. Access tokens are cached in
//! memory and re-fetched only when near expiry.
//!
//! The app registration also carries a `Microsoft Graph` → `Mail.ReadWrite`
//! delegated permission (alongside the Exchange Online IMAP/SMTP ones),
//! requested only when a user explicitly opts a Microsoft 365 account into
//! mirroring Lookout pins to Outlook's own MAPI-property pin (see
//! `graph_pin.rs`) via `ensure_graph_consent`. AAD v2 access tokens are
//! single-resource, so a Graph-audience token is always minted separately
//! from the Exchange-audience one - but the refresh token and the underlying
//! consent grant aren't resource-scoped, so one combined-scope interactive
//! consent is enough to redeem access tokens for either audience afterwards.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use gtk::gio;
use lookout_core::AccountId;
use lookout_mail::session::CredentialProvider;
use lookout_mail::Credential;

/// Client id of Lookout's own public authorization-code flow app in Entra,
/// registered as a public/native client with the `http://localhost` loopback
/// redirect, "Allow public client flows" enabled, and the
/// `IMAP.AccessAsUser.All` / `SMTP.Send` delegated permissions on the
/// Office 365 Exchange Online resource.
const CLIENT_ID: &str = "07725d7c-f588-41ae-bdd0-7ee625fed328";

/// Scopes Exchange Online's IMAP/SMTP endpoints require. A token carrying
/// both works for either protocol (same `outlook.office.com` resource), and
/// `offline_access` makes the token endpoint return a refresh token. This is
/// the only scope list the *implicit* re-auth path (`access_token`, used by
/// the IMAP/SMTP session) ever requests - it must never silently grow to
/// include Graph, since that path can fire in the background (token expiry
/// recovery) without the user having asked for anything new.
const EXCHANGE_SCOPES: &str = concat!(
    "offline_access ",
    "https://outlook.office.com/IMAP.AccessAsUser.All ",
    "https://outlook.office.com/SMTP.Send"
);

/// The Graph scope needed to write MAPI extended properties
/// (`PR_RENEW_TIME`) on a message, so pinning in Lookout can mirror to
/// Outlook's own pin - see `graph_pin.rs`. Only ever requested as part of
/// `COMBINED_SCOPES` below, via the explicit, Config-triggered
/// `ensure_graph_consent`.
const GRAPH_SCOPES: &str = concat!("offline_access ", "https://graph.microsoft.com/Mail.ReadWrite");

/// Requested only by `ensure_graph_consent`: a single interactive consent
/// covering both resources at once, so the resulting refresh token can mint
/// access tokens for either audience afterwards (a v2.0 refresh token isn't
/// scope-bound, even though each individual access token it mints is
/// single-resource).
const COMBINED_SCOPES: &str = concat!(
    "offline_access ",
    "https://outlook.office.com/IMAP.AccessAsUser.All ",
    "https://outlook.office.com/SMTP.Send ",
    "https://graph.microsoft.com/Mail.ReadWrite"
);

/// Which resource a token is being requested for - AAD v2 access tokens are
/// single-resource, so Exchange (IMAP/SMTP) and Graph tokens are always
/// minted, cached, and refreshed independently even though they can share
/// one refresh token once both have been consented.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenAudience {
    Exchange,
    Graph,
}

impl TokenAudience {
    fn refresh_scopes(self) -> &'static str {
        match self {
            TokenAudience::Exchange => EXCHANGE_SCOPES,
            TokenAudience::Graph => GRAPH_SCOPES,
        }
    }
}

const AUTHORIZE_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";

/// How long the interactive browser step waits for the redirect before
/// giving up.
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);

/// A token response from Microsoft's v2.0 endpoint (`expires_in` is seconds;
/// `refresh_token` is absent for public clients on some flows and rotations).
#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredToken {
    refresh_token: String,
    /// Whether this refresh token has actually been proven to redeem a
    /// Graph-audience access token (set by `ensure_graph_consent` after its
    /// confirmation refresh succeeds, cleared if a later Graph refresh gets
    /// rejected - e.g. an admin pulled the permission). `#[serde(default)]`
    /// so a token file saved before Graph support existed just loads as
    /// `false`, not a parse error.
    #[serde(default)]
    graph_consented: bool,
}

/// Persists one account's refresh token. Access tokens never touch disk.
struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    fn for_account(account_id: &AccountId) -> Self {
        TokenStore {
            path: oauth_dir().join(format!("{}.json", sanitize_account_id(account_id))),
        }
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        TokenStore { path }
    }

    fn load(&self) -> Result<StoredToken, String> {
        let data = std::fs::read(&self.path).map_err(|e| format!("couldn't read stored Microsoft credentials: {e}"))?;
        serde_json::from_slice(&data).map_err(|e| format!("stored Microsoft credentials are corrupt: {e}"))
    }

    fn save(&self, stored: &StoredToken) -> Result<(), String> {
        let dir = self.path.parent().ok_or("no parent directory for the token store")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let data = serde_json::to_vec_pretty(stored).map_err(|e| e.to_string())?;
        // Write atomically (tmp + rename) and with 0600 permissions: a
        // refresh token is as sensitive as a saved password.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn delete(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Outcome of trying to exchange a stored refresh token.
enum RefreshOutcome {
    AccessToken(String),
    ReauthNeeded,
}

/// Why a Graph-audience token request failed. `ConsentRequired` covers both
/// "no token at all" and "a token exists but was never (or no longer)
/// granted `Mail.ReadWrite`" - either way, the caller needs to send the user
/// through `ensure_graph_consent` before this account can mirror pins to
/// Outlook.
pub enum GraphAuthError {
    ConsentRequired,
    Transient(String),
}

impl std::fmt::Display for GraphAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphAuthError::ConsentRequired => write!(f, "Outlook pin sync isn't enabled for this account"),
            GraphAuthError::Transient(message) => write!(f, "{message}"),
        }
    }
}

/// Manages one Microsoft 365 account's access tokens, running the
/// interactive browser flow on first use (or on explicit Graph consent) and
/// refresh-token exchanges afterwards. Exchange and Graph audiences are
/// cached and refreshed independently (see `TokenAudience`) but share one
/// on-disk refresh token once both have been consented.
pub struct MicrosoftOAuth {
    account_id: AccountId,
    cached_exchange: Mutex<Option<(String, Instant)>>,
    cached_graph: Mutex<Option<(String, Instant)>>,
}

impl MicrosoftOAuth {
    pub fn new(account_id: AccountId) -> Self {
        MicrosoftOAuth {
            account_id,
            cached_exchange: Mutex::new(None),
            cached_graph: Mutex::new(None),
        }
    }

    fn cache_slot(&self, audience: TokenAudience) -> &Mutex<Option<(String, Instant)>> {
        match audience {
            TokenAudience::Exchange => &self.cached_exchange,
            TokenAudience::Graph => &self.cached_graph,
        }
    }

    /// Returns a fresh Exchange-audience access token (for IMAP/SMTP),
    /// cached until ~a minute before expiry. Behavior is unchanged from
    /// before Graph support existed: a rejected stored refresh token is
    /// permanent, so it's dropped and the account re-authorized against
    /// `EXCHANGE_SCOPES` alone - this path never asks for the Graph
    /// permission on its own.
    pub async fn access_token(&self) -> Result<String, String> {
        if let Some((token, expires_at)) = self.cache_slot(TokenAudience::Exchange).lock().unwrap().clone() {
            if Instant::now() < expires_at {
                return Ok(token);
            }
        }

        let store = TokenStore::for_account(&self.account_id);
        if let Ok(stored) = store.load() {
            match self.refresh(&store, &stored, TokenAudience::Exchange).await? {
                RefreshOutcome::AccessToken(token) => return Ok(token),
                // The token endpoint rejected the stored refresh token
                // (revoked, expired, or issued for a different client id).
                // It can never be fixed by retrying, so drop it and ask the
                // user to authorize again.
                RefreshOutcome::ReauthNeeded => store.delete(),
            }
        }

        self.authorize(&store).await
    }

    /// Returns a fresh Graph-audience access token for `graph_pin.rs`, or
    /// `ConsentRequired` if this account hasn't been through
    /// `ensure_graph_consent` (or the grant no longer works). Never touches
    /// the network to pop an interactive consent screen - this is called
    /// from a background pin action, which must not surprise-launch a
    /// browser.
    pub async fn graph_access_token(&self) -> Result<String, GraphAuthError> {
        if let Some((token, expires_at)) = self.cache_slot(TokenAudience::Graph).lock().unwrap().clone() {
            if Instant::now() < expires_at {
                return Ok(token);
            }
        }

        let store = TokenStore::for_account(&self.account_id);
        let stored = store.load().map_err(|_| GraphAuthError::ConsentRequired)?;
        if !stored.graph_consented {
            return Err(GraphAuthError::ConsentRequired);
        }
        match self.refresh(&store, &stored, TokenAudience::Graph).await {
            Ok(RefreshOutcome::AccessToken(token)) => Ok(token),
            Ok(RefreshOutcome::ReauthNeeded) => {
                // Consent was revoked out of band (e.g. an admin pulled the
                // permission) - clear the flag so Config offers the
                // "Sync pins with Outlook" action again, but leave the
                // refresh token itself alone; the Exchange audience may
                // still work fine off it.
                let _ = store.save(&StoredToken {
                    refresh_token: stored.refresh_token,
                    graph_consented: false,
                });
                Err(GraphAuthError::ConsentRequired)
            }
            Err(e) => Err(GraphAuthError::Transient(e)),
        }
    }

    /// Synchronous, disk-only check for Config UI - never touches the
    /// network, so it's safe to call while just building rows.
    pub fn graph_consented(&self) -> bool {
        TokenStore::for_account(&self.account_id).load().map(|s| s.graph_consented).unwrap_or(false)
    }

    /// Runs the interactive PKCE/loopback consent flow requesting both the
    /// Exchange and Graph scopes at once (`COMBINED_SCOPES`), explicitly
    /// triggered by Config's "Sync pins with Outlook" action - never called
    /// automatically. The resulting refresh token replaces whatever was
    /// stored for this account. Immediately follows up with a Graph-scoped
    /// refresh (rather than trusting whatever audience the code-exchange's
    /// own access token happens to be) to confirm the combined consent
    /// actually redeems a working Graph token on this tenant before
    /// reporting success.
    pub async fn ensure_graph_consent(&self) -> Result<(), String> {
        let store = TokenStore::for_account(&self.account_id);
        let resp = self.authorize_interactive(COMBINED_SCOPES).await?;
        let refresh_token = resp.refresh_token.ok_or("Microsoft token response didn't include a refresh token")?;
        let stored = StoredToken {
            refresh_token,
            graph_consented: true,
        };
        store.save(&stored)?;
        match self.refresh(&store, &stored, TokenAudience::Graph).await {
            Ok(RefreshOutcome::AccessToken(_)) => Ok(()),
            Ok(RefreshOutcome::ReauthNeeded) => {
                // The combined grant didn't actually deliver Graph
                // capability on this tenant - don't leave a false promise
                // behind (the refresh token itself is still kept: it's
                // valid for Exchange either way).
                let _ = store.save(&StoredToken {
                    refresh_token: stored.refresh_token,
                    graph_consented: false,
                });
                Err("Microsoft granted sign-in but didn't allow the Outlook pin-sync permission - check your organization's app consent policy and try again".to_string())
            }
            Err(e) => Err(e),
        }
    }

    async fn refresh(&self, store: &TokenStore, current: &StoredToken, audience: TokenAudience) -> Result<RefreshOutcome, String> {
        let params = [
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", current.refresh_token.as_str()),
            ("scope", audience.refresh_scopes()),
        ];
        let resp = match post_token_request(&params).await {
            // A rejected grant is permanent (the token, or at least this
            // scope of it, is unusable) - the caller decides what that means
            // for its audience; anything else (network, 5xx, unparseable
            // body) is transient and worth retrying later.
            Err(TokenRequestError::Rejected { .. }) => return Ok(RefreshOutcome::ReauthNeeded),
            Err(TokenRequestError::Transient(e)) => return Err(e),
            Ok(resp) => resp,
        };
        if let Some(new_refresh) = resp.refresh_token.filter(|t| !t.is_empty()) {
            // Microsoft rotates refresh tokens; persist the new one (keeping
            // the existing consent flag) so a stale stored token can't
            // trigger a silent re-auth later.
            let _ = store.save(&StoredToken {
                refresh_token: new_refresh,
                graph_consented: current.graph_consented,
            });
        }
        let (access_token, expires_in) = (resp.access_token, resp.expires_in);
        self.cache(audience, access_token.clone(), expires_in);
        Ok(RefreshOutcome::AccessToken(access_token))
    }

    async fn authorize(&self, store: &TokenStore) -> Result<String, String> {
        let resp = self.authorize_interactive(EXCHANGE_SCOPES).await?;
        let refresh_token = resp.refresh_token.ok_or("Microsoft token response didn't include a refresh token")?;
        store.save(&StoredToken {
            refresh_token,
            graph_consented: false,
        })?;
        let (access_token, expires_in) = (resp.access_token, resp.expires_in);
        self.cache(TokenAudience::Exchange, access_token.clone(), expires_in);
        Ok(access_token)
    }

    /// Runs Lookout's interactive PKCE + loopback-redirect authorization
    /// flow against `scope`, exchanging the resulting code for a token.
    /// Doesn't touch the token store or the in-memory cache - callers decide
    /// what to persist, since `ensure_graph_consent`'s combined-scope run
    /// needs different handling than `authorize`'s single-resource one.
    async fn authorize_interactive(&self, scope: &str) -> Result<TokenResponse, String> {
        let verifier = pkce_verifier()?;
        let challenge = pkce_challenge(&verifier);
        let state = uuid::Uuid::new_v4().simple().to_string();

        // Bind the loopback listener first so the redirect URI is already
        // live before the browser opens.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| e.to_string())?;
        let redirect_uri = format!("http://localhost:{}", listener.local_addr().map_err(|e| e.to_string())?.port());

        let authorize_url = format!(
            "{AUTHORIZE_URL}?client_id={}&response_type=code&redirect_uri={}&response_mode=query&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
            percent_encode(CLIENT_ID),
            percent_encode(&redirect_uri),
            percent_encode(scope),
            percent_encode(&state),
            percent_encode(&challenge),
        );
        open_browser(&authorize_url)?;
        tracing::info!("opened a browser for Microsoft OAuth; waiting for the redirect to {redirect_uri}");

        let code = tokio::time::timeout(AUTH_TIMEOUT, receive_auth_code(listener, &state))
            .await
            .map_err(|_| format!("timed out waiting for Microsoft authorization (browser was opened to {authorize_url})"))??;

        let params = [
            ("client_id", CLIENT_ID),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        post_token_request(&params).await.map_err(|e| e.to_string())
    }

    fn cache(&self, audience: TokenAudience, token: String, expires_in: u64) {
        let expires_at = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
        *self.cache_slot(audience).lock().unwrap() = Some((token, expires_at));
    }
}

/// Bridges the OAuth2 flow to `lookout-mail`'s `CredentialProvider` trait.
/// `Send + Sync`: the session's reconnect loop may drive it from its own
/// tokio tasks, unlike the GOA provider (which only ever runs on the single
/// worker task and is wrapped in a `SendWrapper` in `window.rs`).
pub struct MicrosoftCredentialProvider {
    oauth: MicrosoftOAuth,
}

impl MicrosoftCredentialProvider {
    pub fn new(account_id: AccountId) -> Self {
        MicrosoftCredentialProvider {
            oauth: MicrosoftOAuth::new(account_id),
        }
    }
}

#[async_trait::async_trait]
impl CredentialProvider for MicrosoftCredentialProvider {
    async fn imap_credential(&self) -> Result<Credential, String> {
        Ok(Credential::OAuth2AccessToken(self.oauth.access_token().await?))
    }

    async fn smtp_credential(&self) -> Result<Credential, String> {
        Ok(Credential::OAuth2AccessToken(self.oauth.access_token().await?))
    }
}

/// Why a token request failed. `Rejected` means Microsoft refused the grant
/// itself (bad/revoked refresh token, wrong client, denied consent) - retrying
/// will never help; `Transient` covers network and parsing failures.
enum TokenRequestError {
    Rejected { status: u16, body: String },
    Transient(String),
}

impl std::fmt::Display for TokenRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenRequestError::Rejected { status, body } => write!(f, "Microsoft token endpoint returned {status}: {body}"),
            TokenRequestError::Transient(message) => write!(f, "{message}"),
        }
    }
}

async fn post_token_request(params: &[(&str, &str)]) -> Result<TokenResponse, TokenRequestError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| TokenRequestError::Transient(format!("Microsoft OAuth token request failed: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| TokenRequestError::Transient(format!("couldn't read Microsoft token response: {e}")))?;
    if !status.is_success() {
        return Err(TokenRequestError::Rejected { status: status.as_u16(), body });
    }
    serde_json::from_str(&body).map_err(|e| TokenRequestError::Transient(format!("couldn't parse Microsoft token response: {e} ({body})")))
}

/// Serves the loopback redirect until the browser delivers the
/// authorization code, then tells the user to close the window. Extra
/// requests (favicons, reloads) are answered and ignored; `state` is checked
/// to reject forged redirects.
async fn receive_auth_code(listener: tokio::net::TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let request = match read_request_until_headers(&mut sock).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let path = request.split_whitespace().nth(1).unwrap_or("/");
        let Some(query) = path.split_once('?').map(|(_, q)| q) else {
            let _ = respond_html(&mut sock, "No authorization code received; close this window.").await;
            continue;
        };
        let params = parse_query(query);

        if let Some(received_state) = params.get("state") {
            if received_state != state {
                let _ = respond_html(&mut sock, "Authorization state mismatch; close this window and try again.").await;
                continue;
            }
        }
        if let Some(error) = params.get("error") {
            let _ = respond_html(&mut sock, "Microsoft sign-in was cancelled or failed.").await;
            return Err(format!("Microsoft authorization failed: {error}"));
        }
        if let Some(code) = params.get("code") {
            let _ = respond_html(&mut sock, "Authorization complete - you can close this window and return to Lookout.").await;
            return Ok(code.clone());
        }
        let _ = respond_html(&mut sock, "No authorization code received; close this window.").await;
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

fn open_browser(url: &str) -> Result<(), String> {
    gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>).map_err(|e| format!("couldn't open a browser to authorize this Microsoft account: {e}"))
}

fn oauth_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("oauth")
}

/// GOA account ids are D-Bus object paths (e.g.
/// `/org/gnome/OnlineAccounts/Accounts/account_1234`); sanitize into a bare
/// filename.
fn sanitize_account_id(account_id: &AccountId) -> String {
    account_id
        .0
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn pkce_verifier() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("couldn't gather randomness for Microsoft OAuth: {e}"))?;
    Ok(base64url_encode(&bytes))
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64url_encode(&Sha256::digest(verifier.as_bytes()))
}

fn base64url_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 3986 unreserved characters pass through; everything else becomes
/// `%XX`. Covers the space/slash/colon in the scope string.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes an `application/x-www-form-urlencoded` query value (`%XX` escapes
/// and `+` as space).
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(
            percent_encode("https://outlook.office.com/IMAP.AccessAsUser.All"),
            "https%3A%2F%2Foutlook.office.com%2FIMAP.AccessAsUser.All"
        );
    }

    #[test]
    fn url_decode_handles_percent_escapes_and_plus() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_decode("%2F%3A%2F"), "/:/");
        assert_eq!(url_decode("plain"), "plain");
    }

    #[test]
    fn parse_query_extracts_and_decodes_pairs() {
        let params = parse_query("code=abc%2B1&state=xyz&empty");
        assert_eq!(params.get("code").map(String::as_str), Some("abc+1"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
        assert_eq!(params.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn pkce_verifier_and_challenge_are_spec_shaped() {
        let verifier = pkce_verifier().unwrap();
        assert!(verifier.len() == 43, "RFC 7636 verifier must be 43-128 chars, got {}", verifier.len());
        let challenge = pkce_challenge(&verifier);
        assert!(challenge.len() == 43, "S256 challenge must be 43 chars, got {}", challenge.len());
        // Deterministic: same verifier always maps to the same challenge.
        assert_eq!(challenge, pkce_challenge(&verifier));
    }

    #[test]
    fn token_store_round_trips_with_restrictive_permissions() {
        let dir = std::env::temp_dir().join(format!("lookout-oauth-test-{}", uuid::Uuid::new_v4().simple()));
        let store = TokenStore::at(dir.join("acct.json"));
        store
            .save(&StoredToken {
                refresh_token: "rt-123".to_string(),
                graph_consented: true,
            })
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.refresh_token, "rt-123");
        assert!(loaded.graph_consented);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "refresh tokens must be stored 0600");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stored_token_without_graph_consented_field_defaults_to_false() {
        // A token file saved before Graph support existed has no
        // `graph_consented` key at all - it must still load rather than
        // fail to parse, and default to "not consented".
        let dir = std::env::temp_dir().join(format!("lookout-oauth-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acct.json");
        std::fs::write(&path, r#"{"refresh_token":"rt-legacy"}"#).unwrap();
        let store = TokenStore::at(path);
        let loaded = store.load().unwrap();
        assert_eq!(loaded.refresh_token, "rt-legacy");
        assert!(!loaded.graph_consented);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_audience_selects_distinct_scopes() {
        assert_eq!(TokenAudience::Exchange.refresh_scopes(), EXCHANGE_SCOPES);
        assert_eq!(TokenAudience::Graph.refresh_scopes(), GRAPH_SCOPES);
        assert_ne!(TokenAudience::Exchange.refresh_scopes(), TokenAudience::Graph.refresh_scopes());
    }

    #[test]
    fn sanitize_account_id_drops_path_characters() {
        let id = AccountId("/org/gnome/OnlineAccounts/Accounts/account_42".to_string());
        assert_eq!(sanitize_account_id(&id), "_org_gnome_OnlineAccounts_Accounts_account_42");
    }
}
