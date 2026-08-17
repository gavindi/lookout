//! Lookout-managed Google Tasks support.
//!
//! Google's CalDAV endpoint is `VEVENT`-only, so the app's `VTODO` path can
//! never store tasks on Google accounts. The Google Tasks REST API
//! (`tasks.googleapis.com`, what Google's own Tasks uses) is a separate
//! surface with its own OAuth scope, so this module brings its own OAuth2
//! flow - the same public-client authorization-code + PKCE + loopback-redirect
//! pattern as `microsoft_oauth.rs` - because GOA's Google accounts only carry
//! the calendar/mail scopes it requested at account creation.
//!
//! Google's OAuth requires a client id registered in Google Cloud (there is
//! no well-known public client id, unlike Microsoft). Lookout reads it from
//! the `LOOKOUT_GOOGLE_TASKS_CLIENT_ID` environment variable or from
//! `$XDG_CONFIG_HOME/lookout/google-tasks-oauth.json` (a small
//! `{"client_id": "..."}` file the Config view can write); the interactive
//! connect fails with instructions when neither is set.
//!
//! A token per Google account (keyed by its email) persists under
//! `$XDG_DATA_HOME/lookout/oauth/googletasks-<email>.json` with 0600
//! permissions; the last synced lists/tasks cache under
//! `$XDG_CACHE_HOME/lookout/googletasks/<email>.json` for a fast first paint.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Utc};
use gtk::gio;
use lookout_core::{CalendarId, CalendarTask, TaskPriority, TaskStatus, TaskUid};

/// The scope the Tasks API requires. `offline_access` makes the token
/// endpoint return a refresh token.
const SCOPES: &str = "https://www.googleapis.com/auth/tasks offline_access";

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API_BASE: &str = "https://tasks.googleapis.com/tasks/v1";

/// How long the interactive browser step waits for the redirect before
/// giving up.
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the actor re-polls the API while idle (the CalDAV cadence).
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredToken {
    refresh_token: String,
}

struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    fn for_email(email: &str) -> Self {
        TokenStore {
            path: oauth_dir().join(format!("googletasks-{}.json", sanitize_key(email))),
        }
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        TokenStore { path }
    }

    fn exists(&self) -> bool {
        self.path.exists()
    }

    fn load(&self) -> Result<StoredToken, String> {
        let data = std::fs::read(&self.path).map_err(|e| format!("couldn't read stored Google Tasks credentials: {e}"))?;
        serde_json::from_slice(&data).map_err(|e| format!("stored Google Tasks credentials are corrupt: {e}"))
    }

    fn save(&self, stored: &StoredToken) -> Result<(), String> {
        let dir = self.path.parent().ok_or("no parent directory for the token store")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let data = serde_json::to_vec_pretty(stored).map_err(|e| e.to_string())?;
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

/// Whether a stored (refreshable) token exists for `email` - the signal the
/// app uses to auto-connect an account at startup without popping a browser.
pub fn has_stored_token(email: &str) -> bool {
    TokenStore::for_email(email).exists()
}

enum RefreshOutcome {
    AccessToken(String),
    ReauthNeeded,
}

/// Manages one Google account's access token: interactive browser flow on
/// first connect, refresh-token exchange afterwards.
pub struct GoogleTasksOAuth {
    email: String,
    cached: Mutex<Option<(String, Instant)>>,
}

impl GoogleTasksOAuth {
    pub fn new(email: &str) -> Self {
        GoogleTasksOAuth {
            email: email.to_string(),
            cached: Mutex::new(None),
        }
    }

    /// Returns a fresh access token, running the interactive browser flow
    /// when nothing is stored or the stored refresh token was rejected.
    pub async fn access_token(&self) -> Result<String, String> {
        match self.try_access_token().await? {
            Some(token) => Ok(token),
            None => self.authorize(&TokenStore::for_email(&self.email)).await,
        }
    }

    /// Returns a fresh access token using only the in-memory cache and the
    /// stored refresh token. `Ok(None)` means re-authorization is needed
    /// (nothing stored, or the stored refresh token was rejected) - the
    /// caller must not open a browser from a background poll.
    pub async fn try_access_token(&self) -> Result<Option<String>, String> {
        if let Some((token, expires_at)) = self.cached.lock().unwrap().clone() {
            if Instant::now() < expires_at {
                return Ok(Some(token));
            }
        }
        let store = TokenStore::for_email(&self.email);
        if !store.exists() {
            return Ok(None);
        }
        let stored = store.load()?;
        match self.refresh(&store, &stored.refresh_token).await? {
            RefreshOutcome::AccessToken(token) => Ok(Some(token)),
            RefreshOutcome::ReauthNeeded => {
                store.delete();
                Ok(None)
            }
        }
    }

    async fn refresh(&self, store: &TokenStore, refresh_token: &str) -> Result<RefreshOutcome, String> {
        let client_id = client_id();
        let params = [
            ("client_id", client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", SCOPES),
        ];
        let resp = match post_token_request(&params).await {
            Err(TokenRequestError::Rejected { .. }) => return Ok(RefreshOutcome::ReauthNeeded),
            Err(TokenRequestError::Transient(e)) => return Err(e),
            Ok(resp) => resp,
        };
        if let Some(new_refresh) = resp.refresh_token.filter(|t| !t.is_empty()) {
            let _ = store.save(&StoredToken { refresh_token: new_refresh });
        }
        let (access_token, expires_in) = (resp.access_token, resp.expires_in);
        self.cache(access_token.clone(), expires_in);
        Ok(RefreshOutcome::AccessToken(access_token))
    }

    async fn authorize(&self, store: &TokenStore) -> Result<String, String> {
        let client_id = client_id();
        if client_id.is_empty() {
            return Err(format!(
                "Google Tasks needs an OAuth client id. Register a Desktop OAuth client in Google Cloud and either set the \
                 LOOKOUT_GOOGLE_TASKS_CLIENT_ID environment variable or enter it in Config → Google Tasks (saved to {}).",
                client_id_file_path().display()
            ));
        }
        let verifier = pkce_verifier()?;
        let challenge = pkce_challenge(&verifier);
        let state = uuid::Uuid::new_v4().simple().to_string();

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| e.to_string())?;
        let redirect_uri = format!("http://localhost:{}", listener.local_addr().map_err(|e| e.to_string())?.port());

        let authorize_url = format!(
            "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent",
            percent_encode(&client_id),
            percent_encode(&redirect_uri),
            percent_encode(SCOPES),
            percent_encode(&state),
            percent_encode(&challenge),
        );
        open_browser(&authorize_url)?;
        tracing::info!("opened a browser for Google Tasks authorization; waiting for the redirect to {redirect_uri}");

        let code = tokio::time::timeout(AUTH_TIMEOUT, receive_auth_code(listener, &state))
            .await
            .map_err(|_| format!("timed out waiting for Google Tasks authorization (browser was opened to {authorize_url})"))??;

        let params = [
            ("client_id", client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        let resp = post_token_request(&params).await.map_err(|e| e.to_string())?;
        let refresh_token = resp.refresh_token.ok_or("Google token response didn't include a refresh token")?;
        store.save(&StoredToken { refresh_token })?;
        let (access_token, expires_in) = (resp.access_token, resp.expires_in);
        self.cache(access_token.clone(), expires_in);
        Ok(access_token)
    }

    fn cache(&self, token: String, expires_in: u64) {
        let expires_at = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
        *self.cached.lock().unwrap() = Some((token, expires_at));
    }
}

fn client_id() -> String {
    match std::env::var("LOOKOUT_GOOGLE_TASKS_CLIENT_ID") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => load_client_id_file().unwrap_or_default(),
    }
}

/// `$XDG_CONFIG_HOME/lookout/google-tasks-oauth.json` - the file the Config
/// view's Google Tasks row writes, so the client id survives without an env
/// var (GUI apps aren't launched from a terminal).
pub fn client_id_file_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("google-tasks-oauth.json")
}

fn load_client_id_file() -> Option<String> {
    let data = std::fs::read_to_string(client_id_file_path()).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
    parsed.get("client_id")?.as_str().map(str::to_string).filter(|s| !s.is_empty())
}

/// Persists the client id to the config file. Writes atomically; a failure
/// is surfaced to the caller (the Config view toasts it).
pub fn set_client_id(client_id: &str) -> Result<(), String> {
    let path = client_id_file_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_vec_pretty(&serde_json::json!({ "client_id": client_id })).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// The currently-configured client id (env var or file), for display in the
/// Config view.
pub fn configured_client_id() -> String {
    client_id()
}

enum TokenRequestError {
    Rejected { status: u16, body: String },
    Transient(String),
}

impl std::fmt::Display for TokenRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenRequestError::Rejected { status, body } => write!(f, "Google token endpoint returned {status}: {body}"),
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
        .map_err(|e| TokenRequestError::Transient(format!("Google OAuth token request failed: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| TokenRequestError::Transient(format!("couldn't read Google token response: {e}")))?;
    if !status.is_success() {
        return Err(TokenRequestError::Rejected { status: status.as_u16(), body });
    }
    serde_json::from_str(&body).map_err(|e| TokenRequestError::Transient(format!("couldn't parse Google token response: {e} ({body})")))
}

// ---------------------------------------------------------------------------
// REST client
// ---------------------------------------------------------------------------

/// One Google Tasks task list - the Tasks view's "calendar" for
/// colour/grouping purposes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TaskList {
    pub id: String,
    pub title: String,
}

/// A raw task as the API reports it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GoogleTask {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// "needsAction" | "completed".
    #[serde(default)]
    pub status: String,
    /// RFC 3339 with milliseconds, e.g. "2026-08-09T00:00:00.000Z".
    #[serde(default)]
    pub due: Option<String>,
    #[serde(default)]
    pub completed: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub hidden: bool,
}

/// The write body for create (`POST`) / update (`PATCH`) - partial updates
/// are the API's idiom, so unset fields are omitted entirely.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskWrite {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<String>,
}

/// Why an API call failed. `Unauthorized` means the token is dead (revoked or
/// expired beyond refresh) - the actor treats it as re-auth-needed instead of
/// retrying forever.
pub enum ApiError {
    Unauthorized,
    Other(String),
}

pub struct GoogleTasksClient {
    http: reqwest::Client,
}

impl GoogleTasksClient {
    pub fn new() -> Self {
        GoogleTasksClient { http: reqwest::Client::new() }
    }

    async fn send(&self, method: reqwest::Method, url: String, token: &str, body: Option<serde_json::Value>) -> Result<reqwest::Response, ApiError> {
        let mut req = self.http.request(method, url).bearer_auth(token).header("Accept", "application/json");
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body.to_string());
        }
        let resp = req.send().await.map_err(|e| ApiError::Other(format!("Google Tasks request failed: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ApiError::Unauthorized);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::Other(format!("Google Tasks returned {status}: {text}")));
        }
        Ok(resp)
    }

    /// Every task list for the user.
    pub async fn list_task_lists(&self, token: &str) -> Result<Vec<TaskList>, ApiError> {
        let resp = self.send(reqwest::Method::GET, format!("{API_BASE}/users/@me/lists"), token, None).await?;
        let parsed: ListResponse<TaskList> = resp.json().await.map_err(|e| ApiError::Other(format!("couldn't parse task lists: {e}")))?;
        Ok(parsed.items)
    }

    /// Every task in one list, paginated through `nextPageToken` (the API
    /// caps pages at 100). Completed tasks are included; deleted/hidden ones
    /// are dropped.
    pub async fn list_tasks(&self, token: &str, list_id: &str) -> Result<Vec<GoogleTask>, ApiError> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let url = match &page_token {
                Some(pt) => format!(
                    "{API_BASE}/lists/{}/tasks?showCompleted=true&maxResults=100&pageToken={}",
                    percent_encode(list_id),
                    percent_encode(pt)
                ),
                None => format!("{API_BASE}/lists/{}/tasks?showCompleted=true&maxResults=100", percent_encode(list_id)),
            };
            let resp = self.send(reqwest::Method::GET, url, token, None).await?;
            let parsed: ListResponse<GoogleTask> = resp.json().await.map_err(|e| ApiError::Other(format!("couldn't parse task list: {e}")))?;
            out.extend(parsed.items.into_iter().filter(|t| !t.deleted && !t.hidden));
            match parsed.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn create_task(&self, token: &str, list_id: &str, body: &TaskWrite) -> Result<(), ApiError> {
        let payload = serde_json::to_value(body).map_err(|e| ApiError::Other(e.to_string()))?;
        let resp = self
            .send(reqwest::Method::POST, format!("{API_BASE}/lists/{}/tasks", percent_encode(list_id)), token, Some(payload))
            .await?;
        resp.bytes().await.map_err(|e| ApiError::Other(e.to_string()))?;
        Ok(())
    }

    pub async fn update_task(&self, token: &str, list_id: &str, task_id: &str, body: &TaskWrite) -> Result<(), ApiError> {
        let payload = serde_json::to_value(body).map_err(|e| ApiError::Other(e.to_string()))?;
        let url = format!("{API_BASE}/lists/{}/tasks/{}", percent_encode(list_id), percent_encode(task_id));
        let resp = self.send(reqwest::Method::PATCH, url, token, Some(payload)).await?;
        resp.bytes().await.map_err(|e| ApiError::Other(e.to_string()))?;
        Ok(())
    }

    pub async fn delete_task(&self, token: &str, list_id: &str, task_id: &str) -> Result<(), ApiError> {
        let url = format!("{API_BASE}/lists/{}/tasks/{}", percent_encode(list_id), percent_encode(task_id));
        let resp = self.send(reqwest::Method::DELETE, url, token, None).await?;
        resp.bytes().await.map_err(|e| ApiError::Other(e.to_string()))?;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
struct ListResponse<T> {
    items: Vec<T>,
    next_page_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Model mapping
// ---------------------------------------------------------------------------

/// The synthetic calendar id one Google task list maps to - how tasks from
/// every source are distinguished in the merged view.
pub fn google_task_calendar_id(list_id: &str) -> CalendarId {
    CalendarId(format!("googletasks:{list_id}"))
}

/// Extracts the list id from a [`google_task_calendar_id`].
pub fn google_task_list_id(calendar_id: &CalendarId) -> Option<&str> {
    calendar_id.0.strip_prefix("googletasks:")
}

/// Converts one API task into the app's model. Drops the task (None) only
/// when it has no id - title/notes may legitimately be empty.
pub fn convert_task(list_id: &str, gt: &GoogleTask) -> Option<CalendarTask> {
    let completed = gt.status == "completed";
    Some(CalendarTask {
        uid: TaskUid(gt.id.clone()),
        calendar_id: google_task_calendar_id(list_id),
        summary: if gt.title.is_empty() { None } else { Some(gt.title.clone()) },
        description: gt.notes.clone(),
        due: gt.due.as_deref().and_then(parse_rfc3339),
        start: None,
        completed: gt.completed.as_deref().and_then(parse_rfc3339),
        status: if completed { TaskStatus::Completed } else { TaskStatus::NeedsAction },
        priority: TaskPriority(0),
        percent_complete: if completed { Some(100) } else { None },
        categories: Vec::new(),
        href: None,
        etag: None,
    })
}

/// The write body for one app-model task. Google Tasks has no notion of
/// priority, categories, or a percentage - completion is the binary
/// `status`/`completed` pair, everything else maps by name.
pub fn task_to_write(task: &CalendarTask) -> TaskWrite {
    let completed = task.status == TaskStatus::Completed || task.percent_complete == Some(100);
    TaskWrite {
        title: task.summary.clone(),
        notes: task.description.clone(),
        due: task.due.map(format_rfc3339),
        status: Some(if completed { "completed".to_string() } else { "needsAction".to_string() }),
        completed: if completed {
            Some(task.completed.map(format_rfc3339).unwrap_or_else(|| format_rfc3339(Utc::now())))
        } else {
            None
        },
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn format_rfc3339(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// On-disk cache (lists + tasks, fast first paint)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct TasksCache {
    lists: Vec<TaskList>,
    tasks: Vec<CalendarTask>,
}

fn cache_path(email: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("googletasks").join(format!("{}.json", sanitize_key(email)))
}

fn load_cache(email: &str) -> Option<TasksCache> {
    let data = std::fs::read(cache_path(email)).ok()?;
    serde_json::from_slice(&data).ok()
}

fn store_cache(email: &str, lists: &[TaskList], tasks: &[CalendarTask]) {
    let path = cache_path(email);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(data) = serde_json::to_vec(&TasksCache {
        lists: lists.to_vec(),
        tasks: tasks.to_vec(),
    }) else {
        return;
    };
    let _ = std::fs::write(&path, data);
}

// ---------------------------------------------------------------------------
// Sync actor
// ---------------------------------------------------------------------------

pub enum GoogleTasksCommand {
    /// Full resync outside the poll cadence.
    Refresh,
    CreateTask {
        list_id: String,
        task: Box<CalendarTask>,
    },
    UpdateTask {
        list_id: String,
        task: Box<CalendarTask>,
    },
    DeleteTask {
        list_id: String,
        task_id: String,
    },
}

pub enum GoogleTasksEvent {
    ListsUpdated(Vec<TaskList>),
    TasksUpdated(Vec<CalendarTask>),
    /// A problem worth surfacing: connect failure, revoked token, a failed
    /// write, ...
    Error(String),
}

/// Runs one Google account's Tasks sync lifecycle on the calling task (spawn
/// onto the shared worker). Fast-paints the on-disk cache, then polls every
/// `POLL_INTERVAL` and answers write commands immediately, resyncing after
/// each write. Returns (instead of looping forever) when the token is gone -
/// re-authorization is an interactive step only the UI can trigger.
pub async fn run_google_tasks_session(email: String, commands: async_channel::Receiver<GoogleTasksCommand>, events: async_channel::Sender<GoogleTasksEvent>) {
    let oauth = GoogleTasksOAuth::new(&email);
    let client = GoogleTasksClient::new();

    // Fast first paint from the last session's cache.
    if let Some(cache) = load_cache(&email) {
        if !cache.lists.is_empty() {
            let _ = events.send(GoogleTasksEvent::ListsUpdated(cache.lists)).await;
        }
        let _ = events.send(GoogleTasksEvent::TasksUpdated(cache.tasks)).await;
    }

    // The first connect uses the stored refresh token only - never a browser
    // from a background spawn. If nothing is stored (or it was rejected), the
    // session reports it and exits; the UI's explicit connect action runs the
    // interactive flow and re-spawns us.
    let token = match oauth.try_access_token().await {
        Ok(Some(token)) => token,
        Ok(None) => {
            let _ = events
                .send(GoogleTasksEvent::Error(format!("Google Tasks for {email} needs authorization - use Connect Google Tasks")))
                .await;
            return;
        }
        Err(e) => {
            let _ = events.send(GoogleTasksEvent::Error(format!("Google Tasks for {email}: {e}"))).await;
            return;
        }
    };
    let mut token = Some(token);

    loop {
        let Some(current) = &token else { return };
        match sync(&client, &email, current, &events).await {
            Ok(()) => {}
            Err(SessionError::Unauthorized) => {
                let _ = events
                    .send(GoogleTasksEvent::Error(format!(
                        "Google Tasks for {email} is no longer authorized - use Connect Google Tasks to sign in again"
                    )))
                    .await;
                return;
            }
            Err(SessionError::Transient(message)) => {
                tracing::warn!("google tasks sync error for {email}, will retry: {message}");
            }
        }

        enum Wake {
            Poll,
            Command(GoogleTasksCommand),
            ChannelClosed,
        }
        let wake = tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => Wake::Poll,
            c = commands.recv() => match c {
                Ok(cmd) => Wake::Command(cmd),
                Err(_) => Wake::ChannelClosed,
            },
        };
        match wake {
            Wake::Poll => {
                if let Some(new_token) = refresh_token(&oauth).await {
                    token = Some(new_token);
                    if let Some(current) = &token {
                        let _ = sync(&client, &email, current, &events).await;
                    }
                }
            }
            Wake::Command(cmd) => {
                let Some(current) = refresh_token(&oauth).await else { return };
                token = Some(current.clone());
                match cmd {
                    GoogleTasksCommand::Refresh => {}
                    GoogleTasksCommand::CreateTask { list_id, task } => {
                        if let Err(e) = client.create_task(&current, &list_id, &task_to_write(&task)).await {
                            let _ = events
                                .send(GoogleTasksEvent::Error(format!("couldn't save task \"{}\": {}", task.uid, api_error_string(e))))
                                .await;
                        }
                    }
                    GoogleTasksCommand::UpdateTask { list_id, task } => {
                        if let Err(e) = client.update_task(&current, &list_id, &task.uid.0, &task_to_write(&task)).await {
                            let _ = events
                                .send(GoogleTasksEvent::Error(format!("couldn't save task \"{}\": {}", task.uid, api_error_string(e))))
                                .await;
                        }
                    }
                    GoogleTasksCommand::DeleteTask { list_id, task_id } => {
                        if let Err(e) = client.delete_task(&current, &list_id, &task_id).await {
                            let _ = events.send(GoogleTasksEvent::Error(format!("couldn't delete task: {}", api_error_string(e)))).await;
                        }
                    }
                }
                if let Some(current) = &token {
                    let _ = sync(&client, &email, current, &events).await;
                }
            }
            Wake::ChannelClosed => return,
        }
    }
}

enum SessionError {
    Unauthorized,
    Transient(String),
}

/// One full resync: lists, then every list's tasks, emitted as events and
/// written to the cache.
async fn sync(client: &GoogleTasksClient, email: &str, token: &str, events: &async_channel::Sender<GoogleTasksEvent>) -> Result<(), SessionError> {
    let lists = match client.list_task_lists(token).await {
        Ok(lists) => lists,
        Err(ApiError::Unauthorized) => return Err(SessionError::Unauthorized),
        Err(ApiError::Other(e)) => return Err(SessionError::Transient(e)),
    };
    let _ = events.send(GoogleTasksEvent::ListsUpdated(lists.clone())).await;

    let mut tasks = Vec::new();
    for list in &lists {
        match client.list_tasks(token, &list.id).await {
            Ok(items) => {
                tasks.extend(items.iter().filter_map(|t| convert_task(&list.id, t)));
            }
            Err(ApiError::Unauthorized) => return Err(SessionError::Unauthorized),
            Err(ApiError::Other(e)) => {
                tracing::warn!("failed to fetch tasks for list {:?}: {e}", list.title);
            }
        }
    }
    let _ = events.send(GoogleTasksEvent::TasksUpdated(tasks.clone())).await;
    store_cache(email, &lists, &tasks);
    Ok(())
}

/// Refreshes the access token, dropping the session when re-auth is needed.
async fn refresh_token(oauth: &GoogleTasksOAuth) -> Option<String> {
    match oauth.try_access_token().await {
        Ok(Some(token)) => Some(token),
        _ => None,
    }
}

fn api_error_string(e: ApiError) -> String {
    match e {
        ApiError::Unauthorized => "not authorized".to_string(),
        ApiError::Other(message) => message,
    }
}

// ---------------------------------------------------------------------------
// Shared OAuth plumbing (mirrors microsoft_oauth.rs)
// ---------------------------------------------------------------------------

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
            let _ = respond_html(&mut sock, "Google sign-in was cancelled or failed.").await;
            return Err(format!("Google authorization failed: {error}"));
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
    gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>).map_err(|e| format!("couldn't open a browser to authorize Google Tasks: {e}"))
}

fn oauth_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("oauth")
}

/// Emails and Google list ids are URL/path-safe already, but be defensive.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '@' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn pkce_verifier() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("couldn't gather randomness for Google OAuth: {e}"))?;
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
/// `%XX`.
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
        assert_eq!(percent_encode("list/with/slash"), "list%2Fwith%2Fslash");
    }

    #[test]
    fn google_calendar_id_round_trips() {
        let id = google_task_calendar_id("abc123");
        assert_eq!(id.0, "googletasks:abc123");
        assert_eq!(google_task_list_id(&id), Some("abc123"));
        assert_eq!(google_task_list_id(&CalendarId("other".to_string())), None);
    }

    #[test]
    fn converts_api_task_to_model_and_back() {
        let gt = GoogleTask {
            id: "task-1".to_string(),
            title: "Buy milk".to_string(),
            notes: Some("Two litres".to_string()),
            status: "needsAction".to_string(),
            due: Some("2026-08-09T00:00:00.000Z".to_string()),
            completed: None,
            deleted: false,
            hidden: false,
        };
        let task = convert_task("list-1", &gt).expect("converts");
        assert_eq!(task.uid.0, "task-1");
        assert_eq!(task.summary.as_deref(), Some("Buy milk"));
        assert_eq!(task.description.as_deref(), Some("Two litres"));
        assert_eq!(task.due, Some("2026-08-09T00:00:00Z".parse().unwrap()));
        assert_eq!(task.status, TaskStatus::NeedsAction);
        assert_eq!(task.percent_complete, None);
        assert_eq!(task.calendar_id.0, "googletasks:list-1");

        let write = task_to_write(&task);
        assert_eq!(write.title.as_deref(), Some("Buy milk"));
        assert_eq!(write.notes.as_deref(), Some("Two litres"));
        assert_eq!(write.due.as_deref(), Some("2026-08-09T00:00:00.000Z"));
        assert_eq!(write.status.as_deref(), Some("needsAction"));
        assert!(write.completed.is_none());
    }

    #[test]
    fn completed_task_maps_status_percent_and_timestamp() {
        let gt = GoogleTask {
            id: "task-2".to_string(),
            title: "Done".to_string(),
            notes: None,
            status: "completed".to_string(),
            due: None,
            completed: Some("2026-08-07T10:00:00.000Z".to_string()),
            deleted: false,
            hidden: false,
        };
        let task = convert_task("list-1", &gt).expect("converts");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.percent_complete, Some(100));
        assert_eq!(task.completed, Some("2026-08-07T10:00:00Z".parse().unwrap()));

        let write = task_to_write(&task);
        assert_eq!(write.status.as_deref(), Some("completed"));
        assert_eq!(write.completed.as_deref(), Some("2026-08-07T10:00:00.000Z"));
    }

    #[test]
    fn malformed_due_and_completed_dates_drop_gracefully() {
        let gt = GoogleTask {
            id: "task-3".to_string(),
            title: "Bad date".to_string(),
            notes: None,
            status: "needsAction".to_string(),
            due: Some("not-a-date".to_string()),
            completed: Some("also-not".to_string()),
            deleted: false,
            hidden: false,
        };
        let task = convert_task("list-1", &gt).expect("converts");
        assert_eq!(task.due, None);
        assert_eq!(task.completed, None);
    }

    #[test]
    fn token_store_round_trips_with_restrictive_permissions() {
        let dir = std::env::temp_dir().join(format!("lookout-googletasks-test-{}", uuid::Uuid::new_v4().simple()));
        let store = TokenStore::at(dir.join("acct.json"));
        store
            .save(&StoredToken {
                refresh_token: "rt-123".to_string(),
            })
            .unwrap();
        assert!(store.exists());
        let loaded = store.load().unwrap();
        assert_eq!(loaded.refresh_token, "rt-123");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "refresh tokens must be stored 0600");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
