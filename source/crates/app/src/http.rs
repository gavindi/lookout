//! The app's one shared HTTP client, for every request that isn't Graph or
//! DAV: the assistant's endpoints, the Microsoft and Google OAuth token
//! endpoints, Google Tasks and one-click unsubscribe. Sharing it keeps one
//! connection pool and reuses TLS sessions instead of building a fresh client
//! (and handshaking from scratch) per request. Connections are given ten
//! seconds to establish, so a dead host fails promptly even on a request
//! whose own total timeout is minutes long (the assistant's chat calls).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
#[cfg(not(test))]
static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("the shared HTTP client's configuration is valid")
});

/// The shared client. Cloning a `reqwest::Client` only bumps a reference
/// count, so callers take their own handle. The app runs one tokio runtime
/// (the worker's), so pooled connections never outlive theirs; tests build a
/// runtime each, so they get a fresh client to keep a dead runtime's
/// connection out of the next test.
#[cfg(not(test))]
pub(crate) fn client() -> reqwest::Client {
    CLIENT.clone()
}

#[cfg(test)]
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::new()
}
