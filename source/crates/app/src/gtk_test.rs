//! Test-only helper serialising the GTK-initialising test suites.
//!
//! GTK may only be initialised once per process, on a single thread, and
//! libtest hands every `#[test]` its own thread - so the widget-testing
//! suites race to be the one that calls `gtk::init()`. Worse than the
//! usual "whichever loses panics" outcome, two threads calling
//! `gtk::init()` *concurrently* can both pass its "not yet initialized"
//! check, and the first to reach `gdk_display_open_default()` then aborts
//! the whole test process with "gdk_display_open_default() was called
//! before gtk_init()". [`gtk_ready`] closes that window for good: the
//! initialized-checks all happen after acquiring one process-wide lock, so
//! the init itself is serialized, and every test whose thread doesn't own
//! GTK simply skips - the suite then exercises fewer widget tests instead
//! of aborting.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Mutex;

/// The one lock the GTK-initialising tests contend on. Tests only hold it
/// for the check-and-init critical section, never across their bodies - a
/// test whose thread initialized GTK keeps using it without the lock, and
/// every later acquirer sees `gtk::is_initialized()` and skips.
pub static GTK_INIT: Mutex<()> = Mutex::new(());

/// Whether this thread may run a GTK-touching test body: it initialised GTK
/// here, or found it already initialised on this very thread. Callers
/// `return` early on `false`. Skipped rather than failed when the host has
/// no display to initialise against, or when another test thread owns GTK -
/// the same self-skipping convention as the per-module guards this replaces
/// (`window.rs`, `config_view.rs`, `theme.rs`, `message_list.rs`,
/// `calendar_view.rs`), with the race between them finally removed.
pub fn gtk_ready() -> bool {
    let _guard = GTK_INIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if gtk::is_initialized() {
        return gtk::is_initialized_main_thread();
    }
    gtk::init().is_ok()
}
