#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IMAP error: {0}")]
    Imap(#[from] async_imap::error::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] tokio_rustls::rustls::Error),
    #[error("cache error: {0}")]
    Cache(#[from] rusqlite::Error),
    #[error("cache (de)serialization error: {0}")]
    CacheSerde(#[from] serde_json::Error),
    #[error("no usable root certificates found")]
    NoRootCertificates,
    #[error("invalid server hostname: {0}")]
    InvalidServerName(String),
    #[error("login failed: {0}")]
    LoginFailed(String),
    #[error("no {0:?} folder found")]
    NoSuchFolder(lookout_core::MailboxRole),
}

pub type Result<T> = std::result::Result<T, Error>;
