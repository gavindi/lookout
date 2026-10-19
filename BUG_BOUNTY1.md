# Lookout — performance / race / TOCTOU / superfluous-code audit

## Context
An audit of the whole workspace (`source/crates/*`, ~155k lines: app 94k, mail 21k, core 10k, dav 8k, graph 7.5k, vendored async-imap/imap-proto 11k, goa 1k), looking for performance problems, race conditions, superfluous code, TOCTOU and inefficient patterns.

The codebase is already disciplined in many places:
- SQLite runs in WAL mode with transactions and `spawn_blocking`.
- The session tracks the selected mailbox and coalesces body fetches.
- Generation counters guard most async UI updates.
- Graph honours `Retry-After`.
- There is no `Regex::new` in hot paths and no CSS-provider leaks.

The findings below are what remains. Items marked ✔ were verified by reading the code directly; the rest come from the exploration pass with file:line excerpts and are high-confidence.

This document is the report plus a fix list, ordered by severity. Each tier can ship as its own commit.

---

## Tier 1 — Correctness bugs (crash / data loss / duplicates)

1. ✔ **Guaranteed panic: profile-card hover timer.** In [profile_card.rs:162-179](source/crates/app/src/profile_card.rs#L162-L179), the `timeout_add_local_once` closure never clears `timer`.
   - **Failure:** after the card opens, `connect_leave` → `cancel_timer` calls `SourceId::remove()` on a source that has already fired. glib 0.22 `unwrap`s that result, so it panics in a GTK trampoline, which aborts the app.
   - **Fix:** clear the slot inside the closure (`timer_done.borrow_mut().take()`), mirroring the existing fix at [window.rs:9432](source/crates/app/src/window.rs#L9432).
   - **Same pattern elsewhere:** grep every `timeout_add_local_once` / `timeout_add_seconds_local_once` whose `SourceId` is stored and later `.remove()`d, and apply the same fix.

2. ✔ **Duplicate Outlook rules: concurrent rule push.** [graph_sync.rs:420-478](source/crates/app/src/window/graph_sync.rs#L420-L478).
   - **Failure:** `sync_and_push_rules_from_graph(state, account_id)` ignores `account_id` on the push and pushes every account. It is called once per M365 account from [connect.rs:344](source/crates/app/src/window/connect.rs#L344), so N accounts start N overlapping pushes, and each `client.create`s the same missing rule. The pull runs concurrently and can re-import a freshly created rule.
   - **Fix:** make `push_rules_to_graph` take `account_id`, run pull → push sequentially in one future, and add a per-account in-flight `HashSet<AccountId>` on `UiState`.

3. ✔ **Overlapping Outlook property pulls: `sync_mail_properties_from_graph` has no in-flight guard.** [graph_sync.rs](source/crates/app/src/window/graph_sync.rs).
   - **Failure:** six triggers (the connect timer, the 15-minute resync, F5, the global refresh and both consent paths) can overlap. Overlapping runs fetch the same delta page and each makes the session re-emit the message and flagged lists.
   - **Stale cursor:** the session stores the next cursor only after it processes the command, so even a run started just after another finishes can refetch from the old cursor, and a late older run can step the stored cursor backwards.
   - **GTK thread:** the cursor was read with a synchronous SQLite call on the main thread.
   - **Milder than first reported:** the cache's `updated_at <= taken_at` guards stop an older run overwriting newer values, so no data is corrupted.
   - **Fixed:** a per-account `GraphPropertySyncs` on `UiState` (in-flight set plus the newest cursor seen in-process), released by an RAII guard; the cache is read once per account via `gio::spawn_blocking`; the state is forgotten on account removal.

4. ✔ **`settings.json` written non-atomically on the UI thread.** [app_config.rs:722-736](source/crates/app/src/app_config.rs#L722-L736) uses `save_at` with `std::fs::write`, which truncates then writes.
   - **Failure:** a crash mid-save loses all rules, identities, signatures and templates.
   - **Fix:** route it through the shared atomic writer (Tier 3 #12).

5. ✔ **OAuth token store lost updates across instances.**
   - **Where:** [microsoft_oauth.rs:179-193, 245-246, 309-340, 390-417](source/crates/app/src/microsoft_oauth.rs#L179-L193).
   - **Cause:** the refresh lock is per `MicrosoftOAuth` instance, but about 12 instances exist per account (`graph_mail`, `graph_pin`, `graph_rules`, `graph_categories`, `contacts_view`, `connect.rs`…), all sharing one file. Concurrent refreshes race on the fixed `json.tmp` name, so a rename can fail or pick up the other writer's bytes.
   - **Stale consent flag:** `refresh` persists `current.graph_consented`, a stale snapshot, so it can overwrite a concurrent `ensure_graph_consent` that set it to `true`.
   - **Deleted valid token:** on a stale-token rejection `store.delete()` can delete a newer rotated token. Each instance also has its own access-token cache, so the first call from every feature triggers an extra token refresh.
   - **Fix:** a process-wide registry `fn shared(account_id) -> Arc<MicrosoftOAuth>` (a `LazyLock<Mutex<HashMap<AccountId, Weak/Arc<…>>>>`), replacing every `MicrosoftOAuth::new` call site. Re-`load()` inside the lock before any save or delete, and only delete if the stored token still equals the one that was rejected.

6. **Google OAuth has no refresh/authorize serialisation at all.** [google_tasks.rs:138-185](source/crates/app/src/google_tasks.rs#L138-L185).
   - **Failure:** concurrent callers each refresh, and on `None` each run `authorize`, which opens several browser tabs and loopback listeners.
   - **TOCTOU:** `store.exists()` followed by `store.load()?` turns a concurrent delete into a hard error.
   - **Fix:** add a `tokio::sync::Mutex<()>` like the Microsoft version, plus the same shared-registry approach. Drop `exists()` and treat `NotFound` from `load()` as `None`.

7. ✔ **Sender photos stuck missing until restart.** The `sender_photo_cache` ([window.rs:799](source/crates/app/src/window.rs#L799), [state.rs:278-287](source/crates/app/src/window/state.rs#L278-L287)) caches `None` permanently and is never cleared.
   - **Failure:** rows bound before contacts sync never get a photo, edited photos never update, and the cache grows without bound.
   - **Fix:** clear the cache whenever `contacts_by_account` is replaced (contacts snapshot handler), and cap its size.

8. **ICS import can create duplicate UIDs.** [calendar_glue.rs:3083](source/crates/app/src/window/calendar_glue.rs#L3083) / [imip.rs:1153](source/crates/app/src/window/imip.rs#L1153).
   - **Failure:** `find_occurrence_in_calendar` only scans the loaded months, so an event outside that window is treated as new and created again. The scan is also linear per imported event, O(events × occurrences).
   - **Fix:** build a `HashMap<uid, &occ>` once per import, and for misses fall back to a server-side lookup by UID (or the calendar cache's search index).

## Tier 2 — Security-relevant TOCTOU / permissions

9. ✔ **Secrets chmod'd after write.** [microsoft_oauth.rs:186-190](source/crates/app/src/microsoft_oauth.rs#L186-L190) and [google_tasks.rs:94-101](source/crates/app/src/google_tasks.rs#L94-L101) write the token at umask permissions and only then chmod to 0600.
   - **Fix:** create the temp file with `OpenOptions::new().write(true).create_new(true).mode(0o600)` and a unique name.

10. ✔ **Attachments and dragged `.eml` files are world-readable in shared `/tmp`.** [attachments.rs:719-731](source/crates/app/src/window/attachments.rs#L719-L731) and [support.rs:471](source/crates/app/src/window/support.rs#L471) use `std::fs::write` with umask permissions (usually 0644).
    - **Fix:** write into a private `0700` dir under `$XDG_RUNTIME_DIR/lookout/` (falling back to the user cache dir), creating each file with `mode(0o600)` and `create_new`.

11. ✔ **`oauth_dir()` falls back to `temp_dir()`.** [microsoft_oauth.rs](source/crates/app/src/microsoft_oauth.rs) and [google_tasks.rs](source/crates/app/src/google_tasks.rs) put refresh tokens in a predictable, shared `/tmp/lookout/oauth/` when `HOME` and `XDG_DATA_HOME` are unset. Another user could pre-create that directory. `client_id_file_path()` in `google_tasks.rs` had the same fallback for the Google OAuth client id.
    - **Fixed:** both `oauth_dir()`s now use `glib::user_data_dir()`, and `client_id_file_path()` uses `glib::user_config_dir()`. GLib falls back to the passwd home directory when `HOME` is unset (then `/`, which fails safely), never the temp dir.
    - **No migration:** in normal setups (desktop, Flatpak, Snap) GLib resolves to the same path as before, so existing sign-ins are found where they are.
    - **Left for Tier 6 #29:** the five remaining `temp_dir()` fallbacks are disposable caches (`ui_state_db.rs`, `google_tasks.rs` tasks cache, mail and dav caches).

12. ✔ **`write_atomic` temp-name collision.** [mail/src/cache.rs:131-140](source/crates/mail/src/cache.rs#L131-L140) suffixes the temp file with `process::id()`, while its comment claims this keeps concurrent blocking-pool *threads* apart.
    - **Failure:** threads share a pid, so two writers of the same attachment or `.eml` truncate the same tmp file, and the rename can publish a partial file.
    - **Fix:** add a unique suffix (an `AtomicU64` counter or a uuid).
    - **Consolidation:** this becomes the single shared `lookout_core::fsutil::write_atomic(path, bytes, mode)`, which also replaces the hand-rolled copies in `microsoft_oauth.rs`, `google_tasks.rs` (×2), `background.rs` and `app_config.rs`. Also fix `google_tasks.rs:594` `store_cache`, which uses a plain `fs::write`.

## Tier 3 — Main-thread blocking (UI stalls)

13. **Drag prepare does SQLite reads and file writes per dragged message** while holding `state.borrow()`. [support.rs:458-475](source/crates/app/src/window/support.rs#L458-L475).
    - **Fix:** precompute the selection, then load and write on `spawn_blocking`. A GTK drag needs the content synchronously, so either use a `gdk::ContentProvider` with async `write_mime_type_future`, or cap the work and warn.
14. **"Run rules now" does a synchronous `load_messages`** on the UI thread under a borrow. [rules.rs:188-193](source/crates/app/src/rules.rs#L188-L193).
    - **Fix:** use `spawn_cache_read` like every other caller.
15. **Tray enable blocks up to 2 s.** [tray.rs:176-181](source/crates/app/src/tray.rs#L176-L181) calls `recv_timeout`.
    - **Fix:** make it async (`async_channel` and `spawn_future_local`) and update the switch state when it resolves.
16. **File imports and exports run synchronously on the UI thread.**
    - **Where:** `std::fs::read` plus parse in [calendar_glue.rs:3055](source/crates/app/src/window/calendar_glue.rs#L3055) and [contacts_view.rs:946](source/crates/app/src/contacts_view.rs#L946); `std::fs::write` in [contacts_view.rs:1130](source/crates/app/src/contacts_view.rs#L1130).
    - **Fix:** use `gio::File::load_contents_future` and `replace_contents_future`. This also fixes portal and GVfs URIs, where `file.path()` is `None`, and the double parse on import.
17. **Blocking contacts SQLite inside async tasks on the shared tokio runtime.** [contacts_view.rs:2042, 2350-2363, 2443-2456](source/crates/app/src/contacts_view.rs#L2350-L2363).
    - **Cost:** runtime workers shared with the IMAP sessions are tied up. The stale-book cleanup reloads every contact once per stale book.
    - **Fix:** wrap these in `spawn_blocking`, and load contacts once before the loop.

## Tier 4 — Hot-path performance

18. **Mail cache overlays rebuilt on every read.** [mail/src/cache.rs:566-623](source/crates/mail/src/cache.rs#L566-L623), `LocalOverlays::load`.
    - **Cost:** every `load_messages`, `load_summaries` and search call (about 8 sites) scans all four overlay tables into HashMaps. `apply` then computes `follow_up_key` (a `String` allocation) four times per message.
    - **Fix:** compute the key once per summary. For small reads (`load_summaries` with a few uids) query the overlays with `WHERE message_key IN (…)` instead of whole-table loads. Switch the repeatedly used statements to `prepare_cached`.
    - **Fixed:** every overlay read now goes through one `overlay_local` (`load_summaries` included):
      - It computes each summary's `follow_up_key` once and passes it to the four appliers.
      - Reads of up to `OVERLAY_KEYED_MAX` (512) summaries look their rows up by primary key via `message_key IN (SELECT value FROM json_each(?1))`. Larger reads still scan each table once.
      - The overlay SELECTs and the hot read statements use `prepare_cached`, and the connection's statement cache is raised to 64.
    - **Not changed:** `search`, `search_query` and `recent_messages` still parse each row's JSON twice; that is left as a follow-up.
19. **`BodyCache::get` deep-clones `EmailBody`** (HTML that can be megabytes) on every hit. [state.rs:920-925](source/crates/app/src/window/state.rs#L920-L925).
    - **Wasteful callers:** [window.rs:10176](source/crates/app/src/window.rs#L10176) clones just to test `is_some()`; [connect.rs:872-886](source/crates/app/src/window/connect.rs#L872-L886) clones two or three times per `BodyFetched`.
    - **Fix:** store `Rc<EmailBody>` and add `contains()`.
20. **Message-row bind churn.**
    - **Where:** [window.rs:1637-1648](source/crates/app/src/window.rs#L1637-L1648), [1590-1596](source/crates/app/src/window.rs#L1590-L1596), [1561-1574](source/crates/app/src/window.rs#L1561-L1574).
    - **Cost:** every scroll bind recreates the tag-dot `DrawingArea`s, deep-clones the `EmailSummary`, reads GSettings twice and decodes photos synchronously.
    - **Fix:** pre-create `MAX_TAG_DOTS` dot areas and toggle their visibility; store a cheap key (mailbox, uid) or an `Rc` instead of a clone; read the GSettings values once and refresh them on `changed`.
21. **Calendar draw path.** [calendar_view.rs:2785-2881](source/crates/app/src/calendar_view.rs#L2785-L2881).
    - **Cost:** `ellipsize_text` is O(n²), with a `format!` and a `text_extents` call per removed character. `css_color_rgb` reparses the RGBA for every chip on every frame.
    - **Fix:** binary-search the cut point, and parse colours once when the data is set. (Optional: switching cairo toy text to Pango would fix CJK, RTL and emoji rendering; flag it, don't do it in this pass.)
22. **Contacts list rebuilt on every keystroke and every poll.** [contacts_view.rs:1592-1637](source/crates/app/src/contacts_view.rs#L1592-L1637), [2146-2160](source/crates/app/src/contacts_view.rs#L2146-L2160).
    - **Cost:** the search has no debounce. Each rebuild deep-clones every record, and `sort_by_key` lowercases on every comparison.
    - **Fix:** debounce the search by 150 ms, use `sort_by_cached_key`, skip the rebuild when the poll diff is empty, and clone `Rc`s rather than records.
23. **Assistant prompt clones everything.** [window.rs:9547-9557](source/crates/app/src/window.rs#L9547-L9557) clones every occurrence and every contact snapshot, photos included, per prompt.
    - **Fix:** wrap the snapshots in `Arc` in `UiState`.
24. **Per-request `reqwest::Client`.**
    - **Where:** [assistant.rs:93,122,185](source/crates/app/src/assistant.rs#L93), `post_token_request` ([microsoft_oauth.rs:549](source/crates/app/src/microsoft_oauth.rs#L549)), [reading.rs:696](source/crates/app/src/window/reading.rs#L696), [google_tasks.rs:301](source/crates/app/src/google_tasks.rs#L301); each `GraphClient::new` ([graph/src/client.rs:51](source/crates/graph/src/client.rs#L51)) also builds its own.
    - **Fix:** one shared `LazyLock<reqwest::Client>`, which gets connection-pool and TLS-session reuse. Keep dav's per-account client, because it carries the `accept_invalid_certs` setting.
25. **Minor:**
    - `undo_stack.remove(0)` at [mail_actions.rs:2655](source/crates/app/src/window/mail_actions.rs#L2655) should use a `VecDeque`.
    - The calendar search backfill timer ([calendar_search.rs:221-239](source/crates/app/src/window/calendar_search.rs#L221-L239)) needs a generation check so it can't clear a newer search's indicator.

## Tier 5 — Robustness / latent hazards

26. **Edition-2021 `if let Some(x) = state.borrow().field.clone() { x() }`** keeps the `Ref` alive across the callback, which is a latent `BorrowMutError` panic.
    - **Where:** [window.rs:4571](source/crates/app/src/window.rs#L4571), [6740](source/crates/app/src/window.rs#L6740), [reading.rs:1324](source/crates/app/src/window/reading.rs#L1324), [calendar_glue.rs:622, 2337](source/crates/app/src/window/calendar_glue.rs#L622).
    - **Fix:** bind the value with `let` first.
27. **Mutex poisoning cascades.** `self.conn.lock().unwrap()` is used about 70 times in the mail cache, plus the dav cache and the OAuth caches.
    - **Failure:** one panic poisons the lock, and every later cache op panics. In the session that panic surfaces as `spawn_blocking` returning `None`, so the cache dies silently.
    - **Fix:** `lock().unwrap_or_else(PoisonError::into_inner)`; this is safe because SQLite transactions roll back on drop.
28. **`DefaultHasher` for on-disk filenames** ([mail/src/cache.rs:121](source/crates/mail/src/cache.rs#L121)).
    - **Risk:** its algorithm is not guaranteed stable across Rust releases, so a toolchain bump could orphan the attachment and `.eml` caches.
    - **Fix:** use a fixed hash (e.g. FNV-1a, or `sha2`/`blake3` if they are already in the dependency tree).

## Tier 6 — Superfluous / duplicated code

29. **New `lookout_core` helpers to replace duplicates:**
    - `fsutil::{write_atomic, xdg_cache_dir, xdg_data_dir, sanitize_filename}`, replacing `cache_dir` and `sanitize_filename` in mail and dav, `ui_state_db::cache_dir`, `sanitize_account_id` and `sanitize_key`.
    - `first_of_month` / `last_of_month`, replacing 7 copies across mail, dav (×3), graph and app (×2).
    - `escape_like`, replacing the mail and dav copies.
30. **App duplicates to merge:**
    - `open_browser`, `oauth_dir` and `pkce_*` (Microsoft and Google OAuth) → a new `oauth_common.rs`.
    - `open_uri_in_default_browser` (`reading.rs`, `message_window.rs`).
    - `priority_rank` (`tasks_view.rs`, `my_day.rs`).
    - The task-completed check (three copies).
    - `spawn_recent_mail` (`contacts_details.rs`, `profile_card.rs`): keep the version with the generation guard.
    - `parse_addresses` (`identities.rs` vs `rules_view.rs`): unify on the validating one.
31. **Remove `#[allow(dead_code)]` items:**
    - Unused setters in [app_config.rs](source/crates/app/src/app_config.rs#L39) (lines 39, 558, 569, 581, 636): remove, or move them under `#[cfg(test)]` if tests rely on them.
    - `graph_directory.rs:92` `ensure_consent`.
    - The never-constructed `SelectionKind::Section` at [message_list.rs:1119](source/crates/app/src/message_list.rs#L1119).

## Not covered / out of scope
- The vendored `async-imap` and `imap-proto` crates were not audited. The exploration agent for them failed on a network error, so a fork-vs-upstream diff remains a follow-up.
- Rewriting the contacts and tasks lists as model-backed `ListView`s is noted (#22, `tasks_view` full rebuilds) but is too large for this pass.

## Verification
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- New unit tests:
  - `fsutil::write_atomic`: concurrent writers to the same path from 8 threads never yield a partial file, and the created file mode is 0600.
  - The OAuth shared registry returns the same `Arc` per account, and the token store never sees a stale consent flag (simulated with `TokenStore::at`).
  - `BodyCache::contains`/`get` return an `Rc`.
  - `ellipsize_text` matches the old output on fixture strings.
  - Mail cache overlay results are identical before and after the change (existing cache tests plus one for `load_summaries` with overlays).
- Manual checks (via the `run` skill):
  - Hover a sender until the profile card opens, then move away: no crash (Tier 1 #1).
  - With 2 M365 accounts, reconnect and confirm Outlook shows no duplicate rules.
  - Open an attachment and check `stat -c %a` on the temp file is `600`.
  - Toggle the tray with no StatusNotifierWatcher: the UI stays responsive.
  - Type quickly in the People search with many contacts: no stutter.
