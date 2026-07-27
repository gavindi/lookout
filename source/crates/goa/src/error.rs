#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),
    #[error("account does not use this authentication method")]
    WrongAuthMethod,
}

pub type Result<T> = std::result::Result<T, Error>;
