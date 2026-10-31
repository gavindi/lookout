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
//! The app registration also carries `Microsoft Graph` → `Mail.ReadWrite`,
//! `MailboxSettings.ReadWrite`, `Calendars.ReadWrite` and
//! `Calendars.Read.Shared` delegated permissions (alongside the Exchange
//! Online IMAP/SMTP ones), requested only when a user explicitly opts a
//! Microsoft 365 account into Outlook sync (pin mirroring - see `graph_pin.rs` -
//! the category master list - see `graph_categories.rs` - and the calendar
//! backend - OUTLOOK_PARITY.md item 5.1) via `ensure_graph_consent`. AAD v2
//! access tokens are single-resource, so a Graph-audience token is always
//! minted separately from the Exchange-audience one - but the refresh token
//! and the underlying consent grant aren't resource-scoped, so one
//! combined-scope interactive consent is enough to redeem access tokens for
//! either audience afterwards.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use gtk::{gio, glib};
use lookout_core::text::{base64url_encode, percent_encode};
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

/// The Graph scopes Lookout requests for its Microsoft 365 features: writing
/// MAPI extended properties (`PR_RENEW_TIME`) on a message so pinning mirrors
/// to Outlook (`graph_pin.rs`), the category master list
/// (`/me/outlook/masterCategories`, item 2.1 - `MailboxSettings.ReadWrite`),
/// the calendar backend (`Calendars.ReadWrite` for events, item 5.1 /
/// A-03, plus `Calendars.Read.Shared` for free/busy, CAL-19), the contacts
/// backend (`Contacts.ReadWrite`, item 5.2 / A-04), and the To Do backend
/// (`Tasks.ReadWrite`, item 5.3 / A-05). `Place.Read.All` (room
/// finding) and the directory scopes `User.ReadBasic.All`/`People.Read` (GAL,
/// item 5.2 / P-11) are deliberately *not* here: some tenants admin-block
/// them, so they're requested only through `GraphFeature::Rooms` /
/// `GraphFeature::Directory` when the feature is first used, keeping the
/// default consent from breaking. Only ever requested as part of
/// `COMBINED_SCOPES` below, via the explicit, Config-triggered
/// `ensure_graph_consent`. An existing grant made before a scope was added
/// must be re-run from Config to cover it.
const GRAPH_SCOPES: &str = concat!(
    "offline_access ",
    "https://graph.microsoft.com/Mail.ReadWrite ",
    "https://graph.microsoft.com/MailboxSettings.ReadWrite ",
    "https://graph.microsoft.com/Calendars.ReadWrite ",
    "https://graph.microsoft.com/Calendars.Read.Shared ",
    "https://graph.microsoft.com/Contacts.ReadWrite ",
    "https://graph.microsoft.com/Tasks.ReadWrite"
);

/// Requested only by `ensure_graph_consent`: a single interactive consent
/// covering both resources at once, so the resulting refresh token can mint
/// access tokens for either audience afterwards (a v2.0 refresh token isn't
/// scope-bound, even though each individual access token it mints is
/// single-resource). Mirrors [`GRAPH_SCOPES`]'s Graph set plus the Exchange
/// ones, so one Config action opts the account into every Outlook-sync
/// feature (mail properties, categories, calendar, contacts, tasks).
const COMBINED_SCOPES: &str = concat!(
    "offline_access ",
    "https://outlook.office.com/IMAP.AccessAsUser.All ",
    "https://outlook.office.com/SMTP.Send ",
    "https://graph.microsoft.com/Mail.ReadWrite ",
    "https://graph.microsoft.com/MailboxSettings.ReadWrite ",
    "https://graph.microsoft.com/Calendars.ReadWrite ",
    "https://graph.microsoft.com/Calendars.Read.Shared ",
    "https://graph.microsoft.com/Contacts.ReadWrite ",
    "https://graph.microsoft.com/Tasks.ReadWrite"
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
    /// The refresh scope list for this audience. `mail_send` adds Graph's
    /// `Mail.Send` for an account that opted into the Graph mail transport
    /// (item 5.7.6); a token minted without it can read and write mail but
    /// not send, which is all the other Graph features need.
    fn refresh_scopes(self, mail_send: bool) -> Cow<'static, str> {
        match self {
            TokenAudience::Exchange => Cow::Borrowed(EXCHANGE_SCOPES),
            TokenAudience::Graph if mail_send => Cow::Owned(format!("{GRAPH_SCOPES} https://graph.microsoft.com/{}", lookout_graph::MAIL_SEND)),
            TokenAudience::Graph => Cow::Borrowed(GRAPH_SCOPES),
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
    /// Whether this grant also carries Graph's `Mail.Send`, requested only by
    /// the Graph mail transport's opt-in (item 5.7.6). `#[serde(default)]` so
    /// a token file from before the transport loads as `false` and keeps its
    /// original, narrower Graph scope set.
    #[serde(default)]
    mail_send: bool,
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
        let data = serde_json::to_vec_pretty(stored).map_err(|e| e.to_string())?;
        // Atomic and owner-only from the first byte: a refresh token is as
        // sensitive as a saved password.
        lookout_core::fsutil::write_private(&self.path, &data).map_err(|e| e.to_string())
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
/// granted the Graph scopes" - either way, the caller needs to send the user
/// through `ensure_graph_consent` before this account can use any
/// Outlook-sync feature (pin/flag/category mirroring, the calendar backend).
pub enum GraphAuthError {
    ConsentRequired,
    Transient(String),
}

impl std::fmt::Display for GraphAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphAuthError::ConsentRequired => write!(f, "Outlook sync isn't enabled for this account"),
            GraphAuthError::Transient(message) => write!(f, "{message}"),
        }
    }
}

/// The consent flags of an account's stored grant, held in memory so the
/// GTK-thread gates (`graph_consented`, `graph_mail_consented`) don't read and
/// parse the token file on every call.
#[derive(Clone, Copy, Default)]
struct Consent {
    graph: bool,
    mail_send: bool,
}

impl Consent {
    fn of(stored: &StoredToken) -> Self {
        Consent {
            graph: stored.graph_consented,
            mail_send: stored.mail_send,
        }
    }
}

/// Every live [`MicrosoftOAuth`], one per account. Weak, so an account nobody
/// holds any more is dropped without a removal hook, while every caller that
/// asks while one is alive shares it - and with it the refresh locks and the
/// access-token caches.
static REGISTRY: LazyLock<Mutex<HashMap<AccountId, Weak<MicrosoftOAuth>>>> = LazyLock::new(Default::default);

/// Manages one Microsoft 365 account's access tokens, running the
/// interactive browser flow on first use (or on explicit Graph consent) and
/// refresh-token exchanges afterwards. Exchange and Graph audiences are
/// cached and refreshed independently (see `TokenAudience`) but share one
/// on-disk refresh token once both have been consented.
///
/// Obtained through [`MicrosoftOAuth::for_account`], so the IMAP/SMTP
/// session, a shared mailbox borrowing the account's token, the Graph
/// sessions and every Graph feature client share one instance: its locks only
/// serialise refreshes, and its consent cache only stays true, because nobody
/// else in the process writes the same token file.
pub struct MicrosoftOAuth {
    account_id: AccountId,
    store: TokenStore,
    cached_exchange: Mutex<Option<(String, SystemTime)>>,
    cached_graph: Mutex<Option<(String, SystemTime)>>,
    // Serializes `access_token`/`graph_access_token` per audience so two
    // callers racing on an expired token (the mail session's primary
    // connection and its dedicated IDLE connection can now both reconnect
    // around the same time) don't both miss the cache and both fire a
    // concurrent refresh-token exchange - the second one Microsoft would
    // reject via rotation, or which would pop a duplicate browser window if
    // the refresh token was already gone. A distinct lock from the cache
    // storage above: it's held across the whole check-then-refresh sequence
    // (including any network round trip), whereas the storage mutex is only
    // ever held for a quick, synchronous read/write.
    refresh_exchange: tokio::sync::Mutex<()>,
    refresh_graph: tokio::sync::Mutex<()>,
    /// Held around every load-modify-save of the token file (never across a
    /// network call), so the Exchange and Graph refreshes and a consent run
    /// can't overwrite each other's rotation or consent flags.
    store_lock: Mutex<()>,
    /// The stored grant's consent flags, read from disk on first use and kept
    /// in step by every write below. A grant made by a second Lookout process
    /// is only seen after a restart.
    consent: Mutex<Option<Consent>>,
}

impl MicrosoftOAuth {
    /// The account's shared instance, created on first use.
    pub fn for_account(account_id: &AccountId) -> Arc<MicrosoftOAuth> {
        let mut registry = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(live) = registry.get(account_id).and_then(Weak::upgrade) {
            return live;
        }
        registry.retain(|_, weak| weak.strong_count() > 0);
        let oauth = Arc::new(MicrosoftOAuth::new(account_id.clone()));
        registry.insert(account_id.clone(), Arc::downgrade(&oauth));
        oauth
    }

    fn new(account_id: AccountId) -> Self {
        let store = TokenStore::for_account(&account_id);
        MicrosoftOAuth::with_store(account_id, store)
    }

    fn with_store(account_id: AccountId, store: TokenStore) -> Self {
        MicrosoftOAuth {
            account_id,
            store,
            cached_exchange: Mutex::new(None),
            cached_graph: Mutex::new(None),
            refresh_exchange: tokio::sync::Mutex::new(()),
            refresh_graph: tokio::sync::Mutex::new(()),
            store_lock: Mutex::new(()),
            consent: Mutex::new(None),
        }
    }

    /// An instance over an explicit token file, for tests.
    #[cfg(test)]
    fn at(account_id: AccountId, path: PathBuf) -> Self {
        MicrosoftOAuth::with_store(account_id, TokenStore::at(path))
    }

    /// The stored grant's consent flags, from memory after the first read.
    fn consent(&self) -> Consent {
        let mut consent = self.consent.lock().unwrap_or_else(PoisonError::into_inner);
        *consent.get_or_insert_with(|| self.store.load().map(|stored| Consent::of(&stored)).unwrap_or_default())
    }

    fn set_consent(&self, consent: Consent) {
        *self.consent.lock().unwrap_or_else(PoisonError::into_inner) = Some(consent);
    }

    /// Stores a whole new grant (an interactive sign-in or consent).
    fn replace_stored(&self, stored: &StoredToken) -> Result<(), String> {
        let _guard = self.store_lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.store.save(stored)?;
        self.set_consent(Consent::of(stored));
        Ok(())
    }

    /// Re-reads the token file and applies `update` only if it still holds
    /// `expected_refresh_token`, so a refresh or a rejection never writes its
    /// stale view over a grant stored meanwhile, and a deleted file is never
    /// brought back. Returns whether the update was saved.
    fn update_stored_if(&self, expected_refresh_token: &str, update: impl FnOnce(&mut StoredToken)) -> bool {
        let _guard = self.store_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(mut stored) = self.store.load() else { return false };
        if stored.refresh_token != expected_refresh_token {
            return false;
        }
        update(&mut stored);
        match self.store.save(&stored) {
            Ok(()) => {
                self.set_consent(Consent::of(&stored));
                true
            }
            Err(e) => {
                tracing::warn!("couldn't update stored Microsoft credentials for {}: {e}", self.account_id);
                false
            }
        }
    }

    /// Deletes the token file if it still holds the rejected
    /// `refresh_token`. Returns `false` when a newer grant replaced it
    /// meanwhile, which the caller should try instead of re-authorising.
    fn delete_stored_if(&self, refresh_token: &str) -> bool {
        let _guard = self.store_lock.lock().unwrap_or_else(PoisonError::into_inner);
        match self.store.load() {
            Ok(stored) if stored.refresh_token != refresh_token => false,
            _ => {
                self.store.delete();
                self.set_consent(Consent::default());
                true
            }
        }
    }

    /// Persists a rotated refresh token, keeping the consent flags currently
    /// on disk rather than the ones `current` was read with before the round
    /// trip.
    fn persist_rotation(&self, current: &StoredToken, new_refresh: String) {
        self.update_stored_if(&current.refresh_token, |stored| stored.refresh_token = new_refresh);
    }

    fn cache_slot(&self, audience: TokenAudience) -> &Mutex<Option<(String, SystemTime)>> {
        match audience {
            TokenAudience::Exchange => &self.cached_exchange,
            TokenAudience::Graph => &self.cached_graph,
        }
    }

    fn refresh_lock(&self, audience: TokenAudience) -> &tokio::sync::Mutex<()> {
        match audience {
            TokenAudience::Exchange => &self.refresh_exchange,
            TokenAudience::Graph => &self.refresh_graph,
        }
    }

    /// Returns a fresh Exchange-audience access token (for IMAP/SMTP),
    /// cached until ~a minute before expiry. Behavior is unchanged from
    /// before Graph support existed: a rejected stored refresh token is
    /// permanent, so it's dropped and the account re-authorized against
    /// `EXCHANGE_SCOPES` alone - this path never asks for the Graph
    /// permission on its own.
    pub async fn access_token(&self) -> Result<String, String> {
        let _serialize = self.refresh_lock(TokenAudience::Exchange).lock().await;
        if let Some((token, expires_at)) = self.cache_slot(TokenAudience::Exchange).lock().unwrap().clone() {
            if SystemTime::now() < expires_at {
                return Ok(token);
            }
        }

        // At most twice: once for the stored grant, and once more if a
        // newer grant replaced it while the first refresh was rejected.
        for _ in 0..2 {
            let Ok(stored) = self.store.load() else { break };
            match self.refresh(&stored, TokenAudience::Exchange).await? {
                RefreshOutcome::AccessToken(token) => return Ok(token),
                // The token endpoint rejected the stored refresh token
                // (revoked, expired, or issued for a different client id).
                // It can never be fixed by retrying, so drop it and ask the
                // user to authorize again.
                RefreshOutcome::ReauthNeeded => {
                    if self.delete_stored_if(&stored.refresh_token) {
                        break;
                    }
                }
            }
        }

        self.authorize().await
    }

    /// Returns a fresh Graph-audience access token for `graph_pin.rs`, or
    /// `ConsentRequired` if this account hasn't been through
    /// `ensure_graph_consent` (or the grant no longer works). Never touches
    /// the network to pop an interactive consent screen - this is called
    /// from a background pin action, which must not surprise-launch a
    /// browser.
    pub async fn graph_access_token(&self) -> Result<String, GraphAuthError> {
        let _serialize = self.refresh_lock(TokenAudience::Graph).lock().await;
        if let Some((token, expires_at)) = self.cache_slot(TokenAudience::Graph).lock().unwrap().clone() {
            if SystemTime::now() < expires_at {
                return Ok(token);
            }
        }

        let stored = self.store.load().map_err(|_| GraphAuthError::ConsentRequired)?;
        if !stored.graph_consented {
            return Err(GraphAuthError::ConsentRequired);
        }
        match self.refresh(&stored, TokenAudience::Graph).await {
            Ok(RefreshOutcome::AccessToken(token)) => Ok(token),
            Ok(RefreshOutcome::ReauthNeeded) => {
                // Consent was revoked out of band (e.g. an admin pulled the
                // permission) - clear the flag so Config offers the
                // "Sync pins with Outlook" action again, but leave the
                // refresh token itself alone; the Exchange audience may
                // still work fine off it. Skipped if a new grant was stored
                // while this refresh was in flight.
                self.update_stored_if(&stored.refresh_token, |stored| stored.graph_consented = false);
                Err(GraphAuthError::ConsentRequired)
            }
            Err(e) => Err(GraphAuthError::Transient(e)),
        }
    }

    /// Synchronous check for Config UI - never touches the network, and
    /// reads the token file only the first time, so it's safe to call while
    /// just building rows.
    pub fn graph_consented(&self) -> bool {
        self.consent().graph
    }

    /// Whether the stored grant already includes both Graph access and
    /// `Mail.Send`, so enabling Graph mail needs no new browser prompt (item
    /// 5.7.6). In-memory, like [`Self::graph_consented`].
    pub fn graph_mail_consented(&self) -> bool {
        let consent = self.consent();
        consent.graph && consent.mail_send
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
        self.ensure_graph_consent_for(COMBINED_SCOPES).await
    }

    /// Consent for the experimental Graph mail transport (item 5.7.6): the
    /// normal combined Outlook grant plus Graph's `Mail.Send`, which the
    /// transport's outbox needs (`Mail.ReadWrite` alone can write a draft but
    /// not send it). Kept out of [`Self::ensure_graph_consent`] so mail
    /// sending stays an incremental, per-feature consent (§4.4).
    pub async fn ensure_graph_mail_consent(&self) -> Result<(), String> {
        self.ensure_graph_consent_scopes(COMBINED_SCOPES, true).await
    }

    /// The per-feature variant of [`Self::ensure_graph_consent`]: requests
    /// exactly `scopes` (a space-separated OAuth2 scope string) rather than the
    /// fixed combined set, so consent widens incrementally as each Graph
    /// feature is first used (OUTLOOK_PARITY.md §4.4). Only ever called from a
    /// user-initiated action.
    pub async fn ensure_graph_consent_for(&self, scopes: &str) -> Result<(), String> {
        self.ensure_graph_consent_scopes(scopes, false).await
    }

    /// [`Self::ensure_graph_consent_for`], keeping `Mail.Send` once an
    /// account has opted into Graph mail so a later re-consent (say, for
    /// categories) can't silently take sending away.
    async fn ensure_graph_consent_scopes(&self, scopes: &str, mail_send: bool) -> Result<(), String> {
        // Keep Mail.Send once requested, whether by the Graph mail opt-in or
        // by a feature whose own scope set names it (e.g. schedule send).
        let requests_mail_send = scopes.split_whitespace().any(|scope| scope.ends_with("/Mail.Send"));
        let mail_send = mail_send || requests_mail_send || self.consent().mail_send;
        let scopes = if mail_send && !requests_mail_send {
            format!("{scopes} https://graph.microsoft.com/{}", lookout_graph::MAIL_SEND)
        } else {
            scopes.to_string()
        };
        let resp = self.authorize_interactive(&scopes).await?;
        let refresh_token = resp.refresh_token.ok_or("Microsoft token response didn't include a refresh token")?;
        let stored = StoredToken {
            refresh_token,
            graph_consented: true,
            mail_send,
        };
        self.replace_stored(&stored)?;
        match self.refresh(&stored, TokenAudience::Graph).await {
            Ok(RefreshOutcome::AccessToken(_)) => Ok(()),
            Ok(RefreshOutcome::ReauthNeeded) => {
                // The combined grant didn't actually deliver Graph
                // capability on this tenant - don't leave a false promise
                // behind (the refresh token itself is still kept: it's
                // valid for Exchange either way).
                self.update_stored_if(&stored.refresh_token, |stored| {
                    stored.graph_consented = false;
                    stored.mail_send = false;
                });
                Err("Microsoft granted sign-in but didn't allow the Outlook permissions requested - check your organization's app consent policy and try again".to_string())
            }
            Err(e) => Err(e),
        }
    }

    async fn refresh(&self, current: &StoredToken, audience: TokenAudience) -> Result<RefreshOutcome, String> {
        let scopes = audience.refresh_scopes(current.mail_send);
        let params = [
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", current.refresh_token.as_str()),
            ("scope", scopes.as_ref()),
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
            // Microsoft rotates refresh tokens; persist the new one so a
            // stale stored token can't trigger a silent re-auth later.
            self.persist_rotation(current, new_refresh);
        }
        let (access_token, expires_in) = (resp.access_token, resp.expires_in);
        self.cache(audience, access_token.clone(), expires_in);
        Ok(RefreshOutcome::AccessToken(access_token))
    }

    async fn authorize(&self) -> Result<String, String> {
        let resp = self.authorize_interactive(EXCHANGE_SCOPES).await?;
        let refresh_token = resp.refresh_token.ok_or("Microsoft token response didn't include a refresh token")?;
        self.replace_stored(&StoredToken {
            refresh_token,
            graph_consented: false,
            mail_send: false,
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

        let code = tokio::time::timeout(AUTH_TIMEOUT, crate::oauth_loopback::receive_auth_code(listener, &state, "Microsoft"))
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
        // Wall-clock, not `Instant`: the monotonic clock does not advance
        // while the machine is suspended, so an `Instant`-based expiry still
        // looked valid for up to an hour after a laptop resumed past the
        // token's lifetime.
        let expires_at = SystemTime::now() + Duration::from_secs(expires_in.saturating_sub(60));
        *self.cache_slot(audience).lock().unwrap() = Some((token, expires_at));
    }

    /// Drops the cached Exchange-audience token after IMAP/SMTP rejected it,
    /// so the next credential fetch refreshes instead of replaying the same
    /// stale token.
    pub async fn invalidate_exchange_token(&self) {
        *self.cached_exchange.lock().unwrap() = None;
    }

    /// Drops the cached Graph-audience token after a `401`, so the next
    /// `graph_token()` refreshes from the stored refresh token.
    pub async fn invalidate_graph_token(&self) {
        *self.cached_graph.lock().unwrap() = None;
    }
}

/// Bridges the OAuth2 token store to `lookout-graph`'s transport: the
/// [`lookout_graph::TokenProvider`] implementation the Graph client uses.
/// `graph_token` reuses the non-interactive [`MicrosoftOAuth::graph_access_token`]
/// (it never opens a browser); `ensure_scopes` is the only method that runs the
/// interactive flow, so it must only be reached from a user-initiated action.
#[async_trait::async_trait]
impl lookout_graph::TokenProvider for MicrosoftOAuth {
    async fn graph_token(&self) -> Result<String, lookout_graph::GraphError> {
        self.graph_access_token().await.map_err(|error| match error {
            GraphAuthError::ConsentRequired => lookout_graph::GraphError::ConsentRequired(GRAPH_SCOPES.to_string()),
            GraphAuthError::Transient(message) => lookout_graph::GraphError::Token(message),
        })
    }

    async fn ensure_scopes(&self, scopes: &[&str]) -> Result<(), lookout_graph::GraphError> {
        // Build the OAuth2 `scope` string: `offline_access` plus the Graph
        // resource-qualified names, matching `GRAPH_SCOPES`/`COMBINED_SCOPES`.
        let mut requested = String::from("offline_access");
        for scope in scopes {
            requested.push_str(" https://graph.microsoft.com/");
            requested.push_str(scope);
        }
        self.ensure_graph_consent_for(&requested).await.map_err(lookout_graph::GraphError::Token)
    }

    async fn invalidate(&self) {
        self.invalidate_graph_token().await;
    }
}

/// Bridges the OAuth2 flow to `lookout-mail`'s `CredentialProvider` trait.
/// `Send + Sync`: the session's reconnect loop may drive it from its own
/// tokio tasks, unlike the GOA provider (which only ever runs on the single
/// worker task and is wrapped in a `SendWrapper` in `window.rs`).
pub struct MicrosoftCredentialProvider {
    oauth: Arc<MicrosoftOAuth>,
}

impl MicrosoftCredentialProvider {
    /// A shared mailbox passes its parent's id, so it shares the parent's
    /// instance and refreshes the one token under the same lock.
    pub fn new(account_id: &AccountId) -> Self {
        MicrosoftCredentialProvider {
            oauth: MicrosoftOAuth::for_account(account_id),
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

    async fn invalidate_credential(&self) {
        self.oauth.invalidate_exchange_token().await;
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
    let client = crate::http::client();
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

fn open_browser(url: &str) -> Result<(), String> {
    gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>).map_err(|e| format!("couldn't open a browser to authorize this Microsoft account: {e}"))
}

/// `$XDG_DATA_HOME/lookout/oauth` via GLib, which falls back to the passwd
/// home directory when `HOME` is unset - never the shared temp dir, where
/// another local user could pre-create the directory and read or replace
/// the refresh tokens.
fn oauth_dir() -> PathBuf {
    glib::user_data_dir().join("lookout").join("oauth")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Refresh tokens live under the user's data dir, never the shared temp
    /// dir (no env is set here: GLib caches these dirs on first call).
    #[test]
    fn oauth_dir_is_under_the_user_data_dir() {
        let dir = oauth_dir();
        assert_eq!(dir, glib::user_data_dir().join("lookout/oauth"));
        assert!(!dir.starts_with(std::env::temp_dir()));
    }

    /// The cache stores wall-clock expiry, and a rejected token is evicted so
    /// the next lookup refreshes. (Suspend cannot be simulated in a unit test;
    /// this pins the invalidation half and the `SystemTime` type.)
    #[tokio::test]
    async fn invalidating_the_graph_token_evicts_the_cached_access_token() {
        let oauth = MicrosoftOAuth::new(AccountId("acct".to_string()));
        *oauth.cached_graph.lock().unwrap() = Some(("stale-token".to_string(), SystemTime::now() + Duration::from_secs(3600)));
        assert!(oauth.cached_graph.lock().unwrap().is_some());
        oauth.invalidate_graph_token().await;
        assert!(oauth.cached_graph.lock().unwrap().is_none());
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
                mail_send: true,
            })
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.refresh_token, "rt-123");
        assert!(loaded.graph_consented);
        assert!(loaded.mail_send);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "refresh tokens must be stored 0600");

            // A token file left readable by an older build is tightened on
            // the next save rather than keeping its mode.
            std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o644)).unwrap();
            store
                .save(&StoredToken {
                    refresh_token: "rt-456".to_string(),
                    graph_consented: true,
                    mail_send: true,
                })
                .unwrap();
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a resave must tighten a readable token file");
        }
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "acct.json")
            .collect();
        assert!(leftovers.is_empty(), "files left beside the token: {leftovers:?}");
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
        assert!(!loaded.mail_send, "a grant from before Graph mail has no Mail.Send");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_audience_selects_distinct_scopes() {
        assert_eq!(TokenAudience::Exchange.refresh_scopes(false).as_ref(), EXCHANGE_SCOPES);
        assert_eq!(TokenAudience::Graph.refresh_scopes(false).as_ref(), GRAPH_SCOPES);
        assert_ne!(TokenAudience::Exchange.refresh_scopes(false).as_ref(), TokenAudience::Graph.refresh_scopes(false).as_ref());
        // The Graph mail opt-in adds Mail.Send to Graph refreshes only.
        assert_eq!(TokenAudience::Exchange.refresh_scopes(true).as_ref(), EXCHANGE_SCOPES);
        let mail_graph = TokenAudience::Graph.refresh_scopes(true);
        assert!(mail_graph.contains(lookout_graph::MAIL_SEND), "Graph mail needs Mail.Send: {mail_graph}");
        assert!(mail_graph.contains(lookout_graph::MAIL_READ_WRITE), "and the mail read/write base: {mail_graph}");
    }

    #[test]
    fn combined_consent_covers_calendar_and_contacts_but_omits_the_optional_scopes() {
        // The Config "Sync with Outlook" consent must grant the calendar
        // backend its event and free/busy scopes (item 5.1 / A-03, CAL-19)
        // and the contacts backend its contact scope (item 5.2 / A-04)...
        for bare in [
            lookout_graph::CALENDARS_READ_WRITE,
            lookout_graph::CALENDARS_READ_SHARED,
            lookout_graph::CONTACTS_READ_WRITE,
            lookout_graph::TASKS_READ_WRITE,
        ] {
            let qualified = format!("https://graph.microsoft.com/{bare}");
            assert!(GRAPH_SCOPES.contains(qualified.as_str()), "{bare} missing from GRAPH_SCOPES");
            assert!(COMBINED_SCOPES.contains(qualified.as_str()), "{bare} missing from COMBINED_SCOPES");
        }
        // ...but not `Place.Read.All` (room finding) or the directory scopes
        // `User.ReadBasic.All`/`People.Read` (GAL), which some tenants
        // admin-block; they stay per-feature (`GraphFeature::Rooms` /
        // `GraphFeature::Directory`) requests so the default consent never
        // breaks.
        for optional in [lookout_graph::PLACE_READ_ALL, lookout_graph::USER_READ_BASIC_ALL, lookout_graph::PEOPLE_READ] {
            assert!(!GRAPH_SCOPES.contains(optional), "{optional} must not be in GRAPH_SCOPES");
            assert!(!COMBINED_SCOPES.contains(optional), "{optional} must not be in COMBINED_SCOPES");
        }
    }

    fn temp_token_path() -> PathBuf {
        std::env::temp_dir().join(format!("lookout-oauth-test-{}", uuid::Uuid::new_v4().simple())).join("acct.json")
    }

    fn grant(refresh_token: &str, graph_consented: bool, mail_send: bool) -> StoredToken {
        StoredToken {
            refresh_token: refresh_token.to_string(),
            graph_consented,
            mail_send,
        }
    }

    fn remove_temp(path: &std::path::Path) {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Every caller asking while an instance is alive shares it - and so its
    /// refresh locks and caches - and a dropped one is not kept alive.
    #[test]
    fn for_account_shares_one_instance_while_it_is_held() {
        let id = AccountId(format!("registry-test-{}", uuid::Uuid::new_v4().simple()));
        let first = MicrosoftOAuth::for_account(&id);
        let second = MicrosoftOAuth::for_account(&id);
        assert!(Arc::ptr_eq(&first, &second));
        let other = MicrosoftOAuth::for_account(&AccountId(format!("{}-other", id.0)));
        assert!(!Arc::ptr_eq(&first, &other));

        let weak = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(weak.upgrade().is_none(), "the registry must not keep an instance alive");
        let fresh = MicrosoftOAuth::for_account(&id);
        assert_eq!(fresh.account_id, id);
    }

    /// The consent gates read the token file once, then follow this
    /// instance's own writes without touching the disk.
    #[test]
    fn consent_is_read_from_disk_once_and_tracks_this_instances_writes() {
        let path = temp_token_path();
        TokenStore::at(path.clone()).save(&grant("rt-1", true, false)).unwrap();
        let oauth = MicrosoftOAuth::at(AccountId("acct".to_string()), path.clone());
        assert!(oauth.graph_consented());
        assert!(!oauth.graph_mail_consented());

        // A change behind the instance's back is not re-read...
        TokenStore::at(path.clone()).save(&grant("rt-1", false, false)).unwrap();
        assert!(oauth.graph_consented(), "the gate must not re-read the file");

        // ...but its own writes are reflected straight away.
        oauth.replace_stored(&grant("rt-2", true, true)).unwrap();
        assert!(oauth.graph_mail_consented());
        assert!(oauth.delete_stored_if("rt-2"));
        assert!(!oauth.graph_consented());

        let missing = MicrosoftOAuth::at(AccountId("acct".to_string()), temp_token_path());
        assert!(!missing.graph_consented(), "no token file means no consent");
        remove_temp(&path);
    }

    /// A refresh's rotation keeps the flags on disk now, not the ones it read
    /// before its round trip, so it can't revive a consent cleared meanwhile.
    #[test]
    fn a_rotation_keeps_a_consent_flag_cleared_meanwhile() {
        let path = temp_token_path();
        let oauth = MicrosoftOAuth::at(AccountId("acct".to_string()), path.clone());
        oauth.replace_stored(&grant("rt-1", true, true)).unwrap();
        let current = oauth.store.load().unwrap();
        assert!(oauth.update_stored_if("rt-1", |stored| stored.graph_consented = false));

        oauth.persist_rotation(&current, "rt-2".to_string());
        let stored = oauth.store.load().unwrap();
        assert_eq!(stored.refresh_token, "rt-2");
        assert!(!stored.graph_consented, "the rotation brought back a cleared consent flag");
        assert!(stored.mail_send);
        assert!(!oauth.graph_consented());
        remove_temp(&path);
    }

    /// A rejection seen for an old grant neither edits nor deletes a newer
    /// grant stored while the refresh was in flight, and never recreates a
    /// deleted file.
    #[test]
    fn a_rejected_token_does_not_clobber_a_newer_grant() {
        let path = temp_token_path();
        let oauth = MicrosoftOAuth::at(AccountId("acct".to_string()), path.clone());
        oauth.replace_stored(&grant("rt-new", true, false)).unwrap();

        assert!(!oauth.update_stored_if("rt-old", |stored| stored.graph_consented = false));
        assert!(!oauth.delete_stored_if("rt-old"));
        let stored = oauth.store.load().unwrap();
        assert_eq!(stored.refresh_token, "rt-new");
        assert!(stored.graph_consented);

        assert!(oauth.delete_stored_if("rt-new"));
        assert!(!path.exists());
        assert!(!oauth.update_stored_if("rt-new", |stored| stored.graph_consented = true));
        assert!(!path.exists(), "an update must not recreate a deleted token file");
        remove_temp(&path);
    }

    #[test]
    fn sanitize_account_id_drops_path_characters() {
        let id = AccountId("/org/gnome/OnlineAccounts/Accounts/account_42".to_string());
        assert_eq!(sanitize_account_id(&id), "_org_gnome_OnlineAccounts_Accounts_account_42");
    }
}
