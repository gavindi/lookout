/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache error: {0}")]
    Cache(#[from] rusqlite::Error),
    #[error("cache (de)serialization error: {0}")]
    CacheSerde(#[from] serde_json::Error),
    #[error("iCalendar parse error: {0}")]
    Ical(String),
    #[error("recurrence rule error: {0}")]
    Recurrence(String),
    #[error("login failed: {0}")]
    LoginFailed(String),
    #[error("account does not use this authentication method")]
    WrongAuthMethod,
    #[error("CalDAV discovery failed: {0}")]
    Discovery(String),
}

pub type Result<T> = std::result::Result<T, Error>;
