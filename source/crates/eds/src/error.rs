/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),
    /// The source with the mirror's UID exists but doesn't carry Lookout's
    /// marker, so it belongs to something else and must not be changed.
    #[error("EDS source {0} isn't Lookout's mirror (no [Lookout] Mirror=true marker)")]
    SourceNotMirror(String),
    /// The mirror's `.ics` file has to be an absolute path so EDS, which may
    /// run outside Lookout's sandbox, can read it.
    #[error("the mirror file path must be absolute and valid UTF-8: {0}")]
    InvalidMirrorPath(String),
    /// The calendar factory (`Calendar8`) isn't available, so the mirror
    /// source could be created but never read by GNOME Shell.
    #[error("EDS calendars are unavailable")]
    CalendarsUnavailable,
}

impl From<zbus::fdo::Error> for Error {
    fn from(e: zbus::fdo::Error) -> Self {
        Error::DBus(e.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
