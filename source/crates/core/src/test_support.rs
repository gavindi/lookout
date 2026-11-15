//! Keeps test binaries out of the user's real cache directory.
//!
//! Every cache in Lookout lives under `$XDG_CACHE_HOME/lookout/` (or
//! `~/.cache/lookout/` when the variable is unset): the mail and DAV caches,
//! `ui-state.sqlite`, the Google Tasks snapshots. Tests that open one and
//! panic, or that simply never clean up, used to leave their files there for
//! good - 22,656 mail caches and 1,165 calendar caches on one machine
//! (MEMORY_OPTIMISATION1.md, Phase 5).
//!
//! [`isolate_test_cache!`](crate::isolate_test_cache) fixes that once per
//! test binary instead of once per test: before `main` runs it points
//! `XDG_CACHE_HOME` at a directory of the binary's own under the system
//! temporary directory, and removes that directory when the process exits.
//! Every cache path already resolves through the variable, so nothing else
//! has to know about it, and the tests that set `XDG_CACHE_HOME` themselves
//! (`ui_state_db`, `reminders`) keep working as before.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::path::PathBuf;
use std::sync::OnceLock;

static TEST_CACHE_HOME: OnceLock<PathBuf> = OnceLock::new();

extern "C" {
    fn atexit(callback: extern "C" fn()) -> std::os::raw::c_int;
}

extern "C" fn remove_test_cache_home() {
    if let Some(dir) = TEST_CACHE_HOME.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Points `XDG_CACHE_HOME` at `<temp>/lookout-test-cache-<pid>` and arranges
/// for that directory to be removed at exit. Called by
/// [`isolate_test_cache!`](crate::isolate_test_cache)'s constructor before
/// any test thread exists, which is what makes the `set_var` sound; not
/// meant to be called anywhere else.
#[doc(hidden)]
pub fn isolate_cache_home() {
    let dir = std::env::temp_dir().join(format!("lookout-test-cache-{}", std::process::id()));
    if TEST_CACHE_HOME.set(dir.clone()).is_err() {
        return;
    }
    std::env::set_var("XDG_CACHE_HOME", &dir);
    // SAFETY: `atexit` is the C library's; the callback is a plain
    // `extern "C" fn` with no captured state, valid for the whole process.
    unsafe {
        atexit(remove_test_cache_home);
    }
}

/// The cache directory a test binary was given, if
/// [`isolate_test_cache!`](crate::isolate_test_cache) ran in it.
pub fn test_cache_home() -> Option<&'static PathBuf> {
    TEST_CACHE_HOME.get()
}

/// Gives the test binary of the crate (or integration-test file) it is
/// invoked in a private `XDG_CACHE_HOME`, set before `main` runs (see the
/// [module docs](crate::test_support)). Expands to nothing outside
/// `cfg(test)`, so a release build is unaffected. Invoke it once, at the
/// crate root.
#[macro_export]
macro_rules! isolate_test_cache {
    () => {
        #[cfg(test)]
        #[used]
        #[link_section = ".init_array"]
        static LOOKOUT_ISOLATE_TEST_CACHE: extern "C" fn() = {
            extern "C" fn isolate() {
                $crate::test_support::isolate_cache_home();
            }
            isolate
        };
    };
}

// The macro applies to this crate's own test binary too.
crate::isolate_test_cache!();

#[cfg(test)]
mod tests {
    #[test]
    fn test_binaries_get_a_private_cache_home() {
        let dir = super::test_cache_home().expect("the constructor ran before main");
        assert!(dir.starts_with(std::env::temp_dir()), "{dir:?} is under the temporary directory");
        assert_eq!(std::env::var_os("XDG_CACHE_HOME").as_deref(), Some(dir.as_os_str()));
    }
}
