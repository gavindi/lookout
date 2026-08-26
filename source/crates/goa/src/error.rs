/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),
    #[error("account does not use this authentication method")]
    WrongAuthMethod,
}

impl Error {
    /// True when the failure means GNOME Online Accounts simply isn't
    /// registered on the session bus - the expected case on a desktop that
    /// doesn't ship it (KDE has no GOA daemon) - rather than some other
    /// D-Bus or protocol error worth surfacing to the user.
    pub fn is_service_unavailable(&self) -> bool {
        matches!(
            self,
            Error::DBus(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;
