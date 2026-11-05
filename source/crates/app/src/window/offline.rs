//! **Work Offline** (M-40): the app-wide switch and the one signal every
//! network actor listens to.
//!
//! The sender lives on `UiState` for the life of the window. Actors spawn with
//! `offline_receiver`, whose initial value is whatever the switch says at
//! that moment - so an account created while offline never races its first
//! connect. A receiver's `changed()` returns `Err` only if the sender has been
//! dropped; because `UiState` owns it until shutdown, an `Err` means the app
//! is going away, not "wait again", and must never be spun on.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use super::*;

/// A fresh subscription to the app-wide offline signal. Every connect site
/// uses this, so a new account inherits the current switch value rather than
/// assuming online.
pub(crate) fn offline_receiver(state: &Rc<RefCell<UiState>>) -> tokio::sync::watch::Receiver<bool> {
    state.borrow().offline_tx.subscribe()
}

/// Flips the app-wide switch, persists it, tells every actor, and repaints
/// the chrome that shows the state. Going online deliberately seeds nothing:
/// each actor reconnects and syncs itself when its receiver changes.
pub(crate) fn set_work_offline(state: &Rc<RefCell<UiState>>, offline: bool) {
    let (banner, indicator, toggle, toast_overlay, reading_stack, pending_body, cleared, changed, offline_seed) = {
        let mut st = state.borrow_mut();
        let changed = st.work_offline != offline;
        st.work_offline = offline;
        st.settings.set_bool(crate::settings::WORK_OFFLINE, offline);
        let offline_seed = if offline {
            st.calendar_state.as_ref().and_then(|calendar_state| {
                let st = calendar_state.borrow();
                st.calendar_seed_month.clone().map(|seed| (seed, st.displayed_month))
            })
        } else {
            None
        };
        if let Some(calendar_state) = st.calendar_state.as_ref() {
            calendar_state.borrow_mut().work_offline = offline;
        }
        // Tell every actor. `send_replace` updates the stored value even when
        // no actor is subscribed yet, so an account or module created later
        // while still offline inherits the right state through `subscribe()`.
        st.offline_tx.send_replace(offline);
        // A request that was outstanding when the user paused can never be
        // answered while offline. Drop its markers so the next online sync is
        // not deduped against a request that will never land, and turn off any
        // spinners still bound to them.
        let mut cleared = Vec::new();
        if offline {
            cleared.extend(st.syncing.iter().cloned());
            cleared.extend(st.prefetching.iter().cloned());
            st.syncing.clear();
            st.prefetching.clear();
            st.refreshing.clear();
        }
        (
            st.offline_banner.clone(),
            st.offline_indicator.clone(),
            st.offline_toggle.clone(),
            st.toast_overlay.clone(),
            st.reading_stack.clone(),
            st.pending_body_request.clone(),
            cleared,
            changed,
            offline_seed,
        )
    };
    if let Some((seed, month)) = offline_seed {
        seed(month);
    }
    for mailbox in cleared {
        super::mail_actions::refresh_folder_spinner(state, &mailbox);
    }
    if let Some(banner) = banner {
        banner.set_revealed(offline);
    }
    if let Some(indicator) = indicator {
        indicator.set_visible(offline);
    }
    if let Some(toggle) = toggle {
        // The toggled handler guards on `UiState::work_offline`, so this can't
        // recurse: by the time it fires the state already matches.
        toggle.set_active(offline);
    }
    if let (true, Some(stack), Some((mailbox, uid))) = (offline, reading_stack, pending_body) {
        super::reading::show_body_fetch_error(state, &stack, mailbox, uid);
        // The shared failure text names the network; while the network is up
        // but the user paused syncing, say so instead.
        if let Some(page) = stack.child_by_name("error").and_then(|child| child.downcast::<adw::StatusPage>().ok()) {
            page.set_description(Some("Working offline — reconnect to load this message."));
        }
    }
    if changed {
        if let Some(overlay) = toast_overlay {
            overlay.add_toast(adw::Toast::new(if offline { "Working offline" } else { "You're back online" }));
        }
    }
}

#[cfg(test)]
mod tests {
    /// The contract every actor relies on: subscribing while offline starts
    /// at `true`, and `send(false)` wakes a `changed()` waiter.
    #[tokio::test]
    async fn subscribe_sees_the_current_value_and_send_wakes_changed() {
        let (tx, _rx) = tokio::sync::watch::channel(true);
        let mut receiver = tx.subscribe();
        assert!(*receiver.borrow(), "a new subscriber inherits the current value");

        tx.send(false).unwrap();
        receiver.changed().await.expect("the sender is still alive");
        assert!(!*receiver.borrow());
    }

    /// A dropped sender is terminal: `changed()` returns `Err`. Actors must
    /// treat that as "the signal is gone" rather than waiting again.
    #[tokio::test]
    async fn a_dropped_sender_ends_the_wait() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        drop(tx);
        assert!(rx.changed().await.is_err());
    }
}
