# Lookout — second audit: races, TOCTOU, duplication, inefficiency

## Context
This is a second pass over the whole workspace (`source/crates/*`, about 180k lines across 277 `.rs` files). It looked for:
- race conditions and TOCTOU windows
- duplicated code
- general inefficiencies
- security-relevant defects found along the way

The method was targeted reads of the concurrency-heavy paths: the IMAP and Graph session actors, the outbox, the mail/DAV/contacts caches, OAuth, the composer and Graph sync. These were combined with workspace-wide greps for known hazard patterns: existence-then-open checks, non-atomic writes, borrows held across callbacks, stored `SourceId`s, unindexed SQL, and helpers defined more than once.

**Status markers:**
- ✔ — confirmed by reading the cited code.
- ◐ — the mechanism is confirmed, but the user-visible impact depends on runtime behaviour this pass did not exercise (it says what to check).

Line numbers are as of `b081c24` on `dev`.

**Priority tags** (P0 highest):

- **P0** — security, permanent data loss, or a duplicate/erroneous send.
- **P1** — high user-visible cost with a small, low-risk diff.
- **P2** — worth scheduling: real cost, medium diff or a cache-schema change.
- **P3** — do when the surrounding area is already open.
- **P4** — opportunistic, or only worth it if profiling shows the cost.

An item split into halves (#19, #23, #24, #26) carries a tag per half. On a ✔ **Fixed** item the tag records the severity it carried, kept for regression triage.

**Already in good shape** (checked and not reported):
- Every stored `SourceId` that is later `.remove()`d now clears its slot when the timer fires (BUG_BOUNTY1 #1's sweep holds).
- CalDAV/CardDAV and Graph event/contact writes all send `If-Match` / `If-None-Match`.
- SMTP refuses to continue without STARTTLS.
- Both OAuth browser flows are bounded by `AUTH_TIMEOUT`.
- The message list uses minimal `splice` diffs.
- `BodyFetched`, recipient autocomplete and the People directory search are all generation-guarded.

**Tiers** are ordered by severity. Each tier can ship as its own commit. §7 lists the BUG_BOUNTY1 items that are still open.

---

## Tier 1 — Security

1. **P0** ✔ **Fixed** — **ManageSieve falls back to plaintext and sends the password.** [session.rs:6030-6058](source/crates/mail/src/session.rs#L6030-L6058).
   - **Code:** `upload_sieve_script` and `fetch_sieve_capabilities` both do `connect(.., StartTls)`, and on *any* error retry `connect(.., Plain)`, then `authenticate_plain(username, password)`.
   - **Failure:** an on-path attacker only has to strip `STARTTLS` from the capability list ([managesieve.rs:337](source/crates/mail/src/managesieve.rs#L337) then errors with "server does not offer STARTTLS"). The client reconnects in cleartext and sends the account's IMAP password in base64.
   - **Triggers:** this runs on every rule save and every automatic-replies settings open ([graph_sync.rs:759](source/crates/app/src/window/graph_sync.rs#L759)).
   - **Fix:** never fall back to `Plain` for a non-loopback host. Make `Plain` reachable only from tests or `localhost`, and surface "server does not offer STARTTLS" as a `SieveFailed`. If a user really needs it, add an explicit per-account opt-in rather than a silent fallback.
   - **Fixed (0.9.297):** both callers use the new `managesieve::connect_starttls`, which falls back to `Plain` only when `plaintext_fallback_allowed(host)` (localhost or a loopback IP). A server without STARTTLS now fails with `SieveFailed`. Covered by `only_loopback_hosts_may_fall_back_to_plaintext` and `starttls_refuses_a_server_without_the_upgrade`.

2. **P0** ✔ **Fixed** — **OAuth loopback accepts a code with no `state`.** [microsoft_oauth.rs:635-640](source/crates/app/src/microsoft_oauth.rs#L635-L640), [google_tasks.rs:828-833](source/crates/app/src/google_tasks.rs#L828-L833).
   - **Code:** `if let Some(received_state) = params.get("state") { if received_state != state { … } }`. A request that omits `state` skips the check entirely and its `code` is accepted. An `error=` without `state` aborts the flow.
   - **Failure:** any local process that finds the ephemeral port, or a page that makes the browser hit it, can inject its own authorization code (login CSRF: the account gets bound to the attacker's tenant/mailbox) or cancel a sign-in.
   - **Fix:** require `params.get("state") == Some(state)` before looking at `code` or `error`, and ignore everything else.
   - **Hardening:** `read_request_until_headers` ([microsoft_oauth.rs:653](source/crates/app/src/microsoft_oauth.rs#L653)) has no per-connection read timeout. A connection that sends nothing stalls the single accept loop until `AUTH_TIMEOUT`. Wrap each read in a short `tokio::time::timeout`.
   - **Fixed (0.9.297):** the two copies are now one `app/src/oauth_loopback.rs`. `classify_redirect` requires the exact `state` before it looks at `code` or `error`, and each request read is bounded at 10 s. Covered by the module's six tests.

3. **P0** ✔ **Fixed** — **Graph bearer token goes to any absolute URL.** [client.rs:319-326](source/crates/graph/src/client.rs#L319-L326).
   - **Code:** `absolute()` passes any `http(s)://` URL through, and `send_request` attaches `bearer_auth(token)` to it. Next/delta links persisted in the mail cache (`graph_delta_cursor`, `graph_folder_cursors`) are fed back through this path.
   - **Failure:** a tampered cache file, or a malicious redirect target, gets the user's Graph token.
   - **Fix:** only attach the token when the URL's origin equals `base_url`'s (`graph.microsoft.com`). Reject any other host.
   - **Fixed (0.9.297):** `send_request` resolves every URL through `checked_url`, which returns the new `GraphError::UntrustedUrl` unless scheme, host and port match the service root (a different path on the same host is fine). Covered by `the_token_only_goes_to_the_service_roots_origin` and `a_next_link_to_another_host_is_not_followed`.

## Tier 2 — Correctness: duplicate sends, lost edits, deleted mail

4. **P0** ✔ **Send has no re-entrancy guard, so a double-click sends twice.** [compose.rs:2719-2877](source/crates/app/src/compose.rs#L2719-L2877).
   - **Code:** `send_composed` spawns a task that awaits `read_content(&rich_web_view)` (a WebKit round trip), and possibly the attachment-reminder dialog, before it queues `SendMessage`. `closed` and `on_done()` are set only at the end of that task, deliberately (see the comment at 2731-2735). Nothing disables the Send button or marks a send in flight.
   - **Failure:** a second click, or a click plus the ribbon's Send ([compose.rs:2678](source/crates/app/src/compose.rs#L2678)), during that window starts a second task. Both queue `SendMessage`, so the recipient gets the message twice.
   - **Fix:** a `Rc<Cell<bool>> sending` checked and set at the top of `send_composed`, plus `send_button.set_sensitive(false)`. Clear it on the "Don't Send" path and on early returns.
   - **Fixed (0.9.298):** the guard is checked and set synchronously before the editor read, disables the split button, and is released on "Don't Send". GTK's programmatic `emit_by_name` path is blocked by the guard, not by sensitivity.

5. **P1** ◐ **Close can drop the final draft save.** [compose.rs:2687-2700](source/crates/app/src/compose.rs#L2687-L2700).
   - **Code:** Close spawns `autosave.attempt()` and then immediately sets `closed` and calls `on_done()`.
   - **Failure, part 1:** `attempt()` returns early when a save is already in flight ([compose.rs:1068-1075](source/crates/app/src/compose.rs#L1068-L1075)). With a 5 s autosave tick, an in-flight tick is common, so edits made after that tick's snapshot are lost. `flush()` exists for exactly this case and is only used by schedule-send.
   - **Failure, part 2:** the spawned future first runs after `on_done()` has torn the page down. In rich-text mode `snapshot()` then has to read a WebView that the send path's own comment says must still be parented. If `read_content` returns `None`, nothing is saved.
   - **Check:** close a rich-text composer within 5 s of typing, then look in Drafts.
   - **Fix:** `close_button` should `await autosave.flush()` *before* `closed.set(true); on_done()`, in the same task. The send path already does this.
   - **Fixed (0.9.298):** Close sets `closed` idempotently, then awaits `autosave.flush()` in the same task and only then calls `on_done()`, so an in-flight tick is waited out and the rich editor is read while still parented.

6. **P0** ✔ **Outbox row deleted only after more network round trips, so a crash resends.**
   - **Where:** [session.rs:1793-1806](source/crates/mail/src/session.rs#L1793-L1806) (IMAP) and [graph/src/mail/session.rs:840-847](source/crates/graph/src/mail/session.rs#L840-L847) (Graph).
   - **Code:** after SMTP/`sendMail` succeeds, the drain first archives to Sent (IMAP `APPEND`) and deletes the draft (search + store + expunge), and only then calls `delete_outbox(id)`.
   - **Failure:** quitting, crashing, losing the network or suspending in that window leaves the row `pending`, and the next session start sends the message again.
   - **Fix:** delete the row, or mark it `sent` so a later cleanup can archive it, immediately after the transport accepts the message. Then do the best-effort archive and draft cleanup.
   - **Same pass:** batch the post-send work. Today every sent entry triggers its own `refresh_folders_targeted` + full Sent `sync_mailbox` + reselect (IMAP), or a Sent `sync_mailbox` (Graph). Draining an offline outbox of N messages costs N Sent resyncs, where one after the loop would do.
   - **Fixed (0.9.298):** both drains delete the row and emit `OutboxSent` immediately after the transport accepts the message, then do the best-effort archive and draft cleanup. IMAP and Graph both do one Sent resync (and one IMAP reselect) after the whole due batch.

7. **P0** ✔ **"Empty folder" deletes mail that arrived after the user confirmed.** [session.rs:5867-5883](source/crates/mail/src/session.rs#L5867-L5883).
   - **Code:** `empty_mailbox` runs `UID SEARCH ALL` and then `UID STORE 1:* +FLAGS (\Deleted)` + `UID EXPUNGE 1:*`.
   - **Failure:** a message delivered between the user confirming "Empty Junk (12 messages)" and the STORE, or between the SEARCH and the STORE, is permanently deleted unseen. The returned `count` is also wrong then. This matters for Junk, where a misfiled legitimate message is exactly what lands mid-operation.
   - **Fix:** STORE and EXPUNGE the searched set, compressed into ranges (`1:40,42,57:90`) so the command-line-length concern in the comment still holds. Better still, have the UI pass the UID set it showed.
   - **Fixed (0.9.298):** `empty_mailbox` flags and expunges only the `SEARCH ALL` UIDs, compressed with `uid_set_chunks`; the caller drops exactly those UIDs from the cache instead of clearing the folder. The snapshot-boundary test lives in `mail/src/session.rs`.

8. **P1** ✔ **Rules deleted in Outlook come back from Lookout, and the push still races the pull.** [graph_sync.rs:436-552](source/crates/app/src/window/graph_sync.rs#L436-L552).
   - **Resurrection:** `sync_rules_from_graph` only adds and updates; a rule missing remotely is never removed locally. `push_rules_to_graph` then treats any local Graph-backed rule whose id isn't in `remote_ids` as new and calls `client.create`. Deleting a rule in Outlook on the web is undone at the next connect.
   - **Pull overwrites local edits:** `*existing = rule` whenever the remote differs, so a local edit that hasn't been pushed yet is lost.
   - **Concurrency (BUG_BOUNTY1 #2, still open):** `sync_and_push_rules_from_graph` starts the pull and the push as two independent futures, and the push iterates *every* account. With N Microsoft 365 accounts that is N concurrent pushes, and a pull can import a just-created remote rule before the push writes its new id back, which duplicates it locally.
   - **Fix:**
     - Run pull then push sequentially in one future, for that account only, behind a per-account in-flight set (the `GraphPropertySyncs` shape).
     - Keep a per-rule "last pushed" fingerprint or remote id. A rule that has a Graph id and is missing remotely was deleted remotely, so delete it locally. Only rules without a Graph id are created.
   - **Fixed (0.9.298):** `Rule.synced { account, fingerprint }` is persisted in `settings.json`, and the new pure `reconcile_rules` runs pull then push sequentially, per account, behind a `GraphRuleSyncs` in-flight set. A remote-deleted rule is removed locally and not re-created; a pending local edit is pushed instead of overwritten; unchanged rules adopt Outlook's copy; new remote rules import once; non-UUID ids with no marker are migrated as already pushed. Fingerprints come from the content actually sent, so an edit made during the request stays dirty.

9. **P1** ✔ **OAuth access-token expiry uses `Instant`, so it is stale after suspend.** [microsoft_oauth.rs:522-525](source/crates/app/src/microsoft_oauth.rs#L522-L525), [google_tasks.rs:236-239](source/crates/app/src/google_tasks.rs#L236-L239).
   - **Cause:** `Instant` is `CLOCK_MONOTONIC` on Linux, which does not advance while the machine is suspended. After a laptop sleeps past the token's ~60-90 min lifetime, the cache still believes the token is valid for up to its remaining monotonic time.
   - **Failure:** for up to an hour after resume, IMAP/SMTP XOAUTH2 logins and Graph calls fail.
   - **No recovery path:** Graph's `send_request` turns a 401 straight into `GraphError::Unauthorized` without invalidating the cache or retrying ([client.rs:286-312](source/crates/graph/src/client.rs#L286-L312)).
   - **Fix:**
     - Store the expiry as wall-clock time (`SystemTime`/`chrono::Utc`).
     - On a 401, evict the cached access token for that audience and retry once (add `TokenProvider::invalidate`).
     - Do the same for the IMAP session's reconnect after an `AUTHENTICATE` failure.
   - **Fixed (0.9.298):** both Microsoft and Google access-token caches use `SystemTime`; `TokenProvider::invalidate`/`CredentialProvider::invalidate_credential` evict the cached token, `GraphClient` retries once on a 401, and rejected IMAP `AUTHENTICATE`/`LOGIN` (primary and IDLE connections) and SMTP `AUTH` failures evict the Exchange token (new `Error::AuthRejected`).

10. **P2** ✔ **Graph `Retry-After` is uncapped, and it stalls the whole session actor.** [client.rs:17-22, 291-297](source/crates/graph/src/client.rs#L291-L297).
    - **Code:** the doc comment says `MAX_RETRY_DELAY` (120 s) is the "ceiling on any single retry wait", but only the fallback backoff is clamped; a server `Retry-After` is slept verbatim, up to 5 times.
    - **Failure:** the Graph mail session handles commands serially, so one throttled delta or page fetch blocks user moves, sends and body fetches for the whole duration.
    - **Fix:** `retry_after.map(|d| d.min(MAX_RETRY_DELAY))`. For background calls, prefer giving up after the cap and rescheduling over sleeping inside the actor.
    - **Fixed (0.9.298):** retry waits are clamped to `MAX_RETRY_DELAY`, and a `Retry-After` longer than the cap returns `GraphError::Throttled` immediately instead of sleeping inside the session actor. Pinned by `an_over_cap_retry_after_gives_up_without_sleeping`.

## Tier 3 — Races and TOCTOU (stale or overwritten state)

11. **P1** ✔ **Outlook mirror writes can land out of order.** [graph_sync.rs:103-180](source/crates/app/src/window/graph_sync.rs#L103-L180).
    - **Code:** `mirror_follow_up_to_graph` and `mirror_categories_to_graph` (and the pin mirror) each spawn an independent `spawn_future_local` per change. Each one runs `resolve_message_id` and then `PATCH`.
    - **Failure:** flag → unflag, or two quick category edits, are two concurrent PATCH chains whose completion order is up to the network. Outlook can end up with the *older* state, and the next delta pull then copies that older state back over the newer local one.
    - **Fix:** a per-(account, message) serial queue, or a "latest wins" slot. Keep the newest pending value per message and have one worker drain it.
    - **Fixed (0.9.299):** `GraphMirrorQueue` keeps the newest pending value per `(account, Message-ID, property)` and runs one drain worker per key, so flag→unflag and quick category edits are sent in order while distinct properties stay independent; the pin mirror uses the same queue. Covered by the queue's latest-wins bookkeeping test.

12. **P1** ✔ **The flat-file sweep races live writes in the IMAP session.**
    - **Where:** the sweep is in [cache.rs:1608-1650](source/crates/mail/src/cache.rs#L1608-L1650) and is started from [session.rs:1418-1433](source/crates/mail/src/session.rs#L1418-L1433).
    - **Code:** `sweep_orphaned_files` snapshots `known` from `messages`, then walks the attachment and `.eml` directories and deletes every file not in the snapshot. The IMAP session runs it fire-and-forget, concurrently with the first sync and with user-triggered attachment and raw-message fetches.
    - **Failure:** a message synced after the snapshot whose attachment or `.eml` is written before the walk reaches it is deleted right after being written. A caller holding that path, such as drag-out or Open, then fails.
    - **Comparison:** the Graph session already *awaits* maintenance before its first sync for the analogous `graph_uids` race ([graph/src/mail/session.rs:133-158](source/crates/graph/src/mail/session.rs#L133-L158)).
    - **Fix:** await `run_maintenance` before the first sync on IMAP too. Alternatively, skip files whose mtime is newer than the snapshot.
    - **Related:** `purge_message_files` runs *before* `tx.commit()` in `replace_messages` and its siblings ([cache.rs:1154, 1284, 2292](source/crates/mail/src/cache.rs#L1154)). A failed commit leaves rows whose files are gone, and the `read_dir` over the whole attachments directory runs while holding the connection mutex. Move it after the commit and outside the lock.
    - **Fixed (0.9.299):** `run_maintenance` is split into `run_startup_cleanup` (sweep + `graph_uids` prune), which the IMAP session awaits before its first sync, and `run_space_reclaim` (the one-time vacuum), which stays fire-and-forget. `purge_message_files` now runs after `tx.commit()` and after dropping the connection mutex in `replace_messages`, `store_partial_messages` and `delete_messages`.

13. **P2** ✔ **Calendar search index: concurrent month stores, plus a fresh connection per call.** [support.rs:528-600](source/crates/app/src/window/support.rs#L528-L600).
    - **Race:** `spawn_calendar_cache_store` (Graph months) spawns an unordered blocking write per update. Two refreshes of the same month can commit out of order, which leaves the *older* snapshot indexed until the next refresh.
    - **Cost:** store, search and missing-months each call `CalendarCache::open`, which opens a new SQLite connection and re-runs the `PRAGMA`s, the DDL and the `user_version` check per call. That happens per search keystroke and per account.
    - **Fix:** keep one `Arc<CalendarCache>` per account on `CalendarUiState`, and serialise stores per account (one worker channel), or stamp each store with a generation and drop older ones.
    - **Fixed (0.9.299):** a `CalendarCacheSlot` on `CalendarUiState` lazily opens one `Arc<CalendarCache>` per account and shares it across search, the backfill check and the month writer. Stores are generation-stamped on the GTK thread and only the newest request for a month is written under a per-slot write lock; slots are dropped on account removal and cleared with "Clear all caches". Covered by the store-generation test.

14. **P1** ✔ **`BodyCache` is never invalidated.** [state.rs:1131-1156](source/crates/app/src/window/state.rs#L1131-L1156).
    - **Code:** it is keyed by `(MailboxId, Uid)` only, and nothing in the app ever calls `remove`, `retain` or `clear` on it.
    - **Failure:** after a UIDVALIDITY reset (a Dovecot index rebuild, a server migration), the reading pane can show a cached body for a *different* message that now has the same UID. Bodies of removed accounts also stay in memory.
    - **Fix:** include `uidvalidity` in the key, or clear a mailbox's entries when the session reports a validity change, and purge an account's entries on removal.
    - **Fixed (0.9.299):** `BodyCache` gained `remove_mailbox` and `retain`; the `FoldersUpdated` handler compares each mailbox's `UIDVALIDITY` and drops a recreated mailbox's bodies, and account teardown, edit, transport switch and removal call `forget_account_bodies`. Covered by the body-cache invalidation and `uidvalidity_changes` tests.

15. **P0** ✔ **Settings files: a corrupt read becomes defaults, and the next save makes the loss permanent.** [app_config.rs:742-767](source/crates/app/src/app_config.rs#L742-L767), [calendar_colors.rs:55-84](source/crates/app/src/calendar_colors.rs#L55-L84).
    - **Code:** `load_at` turns an unparseable `settings.json` into `AppConfig::default()` with only a warning. The first later `save` then overwrites the file non-atomically with those defaults (BUG_BOUNTY1 #4, still open).
    - **Failure:** a half-written file from a crash, or one hand-edit typo, silently destroys every rule, identity, signature and template.
    - **Fix:** alongside the atomic writer, rename an unparseable file to `settings.json.corrupt-<timestamp>` before falling back, and toast the user. The same applies to `calendar-colors.json`.
    - **Fixed (0.9.299):** a new `fsutil` module provides `write_atomic` (unique `create_new` temp file, `sync_all`, rename, existing mode preserved) and `backup_corrupt`. `settings.json` and `calendar-colors.json` now keep a `.corrupt-<timestamp>` copy before falling back to defaults, show a startup toast naming it, and are written atomically. Covered by the backup and concurrent-writer tests.

16. **P4** ◐ **Directory source: a cache hit or short prefix doesn't cancel the pending lookup.** [directory.rs:204-282](source/crates/app/src/directory.rs#L204-L282).
    - **Code:** `search()` returns early for a cached prefix or one below the minimum length, without bumping `generation` or removing the debounce timer. A query already debounced or in flight therefore still fires, and its `complete` runs with a generation that still matches.
    - **Impact:** both current callers re-check their own generation (`RecipientEntry::apply_suggestions`, the People directory handler), so nothing stale is shown today. It still wastes a GAL/LDAP round trip, and any new caller inherits the bug.
    - **Fix:** in both early-return paths, bump `generation`, `take()` and remove the timer, and clear `pending`.
    - **Fixed (0.9.299):** `cancel_pending` bumps the generation, clears `pending` and removes the debounce timer on the too-short, cache-hit and no-backends paths. Covered by the cancellation test.

## Tier 4 — Inefficiency (CPU, I/O, main thread)

17. **P1** ✔ **Stale address books are cleaned up on every poll, forever.**
    - **Where:** [contacts_view.rs:2734-2750 and 2828-2846](source/crates/app/src/contacts_view.rs#L2734-L2750), with [dav/src/cache.rs:677-697](source/crates/dav/src/cache.rs#L677-L697).
    - **Code:** for each cached book missing server-side, the sync loads and parses *every* cached vCard (`load_contacts`) just to filter one book's hrefs, then calls `apply_delta(&stale.href, None, …)`. But `apply_delta` *upserts* the book row (with `sync_token = NULL`), and nothing ever runs `DELETE FROM address_books`.
    - **Failure:** the stale book is found again on every poll, and every poll re-parses the whole address book once per stale book, indefinitely.
    - **Fix:** add `ContactsCache::remove_book(href)`, one transaction doing `DELETE FROM contacts WHERE book_href = ?` and `DELETE FROM address_books WHERE href = ?`, and call it instead.

18. **P3** ✔ **`ContactsCache` has no WAL and no transactions, and runs on async workers.** [dav/src/cache.rs:561-697](source/crates/dav/src/cache.rs#L561-L697).
    - **Comparison:** the calendar cache in the same file sets WAL, `busy_timeout` and `synchronous=NORMAL`; the contacts cache sets none of them.
    - **Code:** `apply_delta`, `upsert_contacts` and `store_address_books` run one autocommit per row. An initial sync of 5,000 cards is 5,000 journal commits with fsyncs. This runs directly inside the async contacts task (BUG_BOUNTY1 #17, still open), on the shared tokio runtime the IMAP sessions use.
    - **Partial-state window:** `CalendarCache::store_tasks` ([cache.rs:300-305](source/crates/dav/src/cache.rs#L300-L305)) does `DELETE` then `INSERT` without a transaction, so a reader or crash in between sees no tasks.
    - **Fix:** use the same pragmas as `CalendarCache::open`, wrap each method in `conn.transaction()`, use `prepare_cached`, and call the cache through `spawn_blocking`.

19. **P3** (a) / **P4** (b) ✔ **The flagged-mail list is a full-table scan after every sync.**
    - **Where:** the query is [cache.rs:3030-3048](source/crates/mail/src/cache.rs#L3030-L3048); `emit_flagged_messages` is called at [session.rs:7310](source/crates/mail/src/session.rs#L7310), 4471 and 4491.
    - **Code:** `flagged_messages` runs `WHERE data LIKE '%$Lookout-flag%' OR trim(json_extract(data,'$.message_id'),'<> ') IN (…)`. That is a substring scan of every row's JSON plus a JSON parse for every row that misses the `LIKE`. It runs at the end of *every* `sync_mailbox`, so every IDLE wake and poll, plus every flag change and every Graph property apply.
    - **Cost:** O(cached messages) per wake on a 100k-message cache.
    - **Fix:**
      - Persist a `flagged INTEGER` column (or a `flagged_keys` table) maintained on upsert, with a partial index.
      - Emit `FlaggedMessages` only when a sync actually changed a flag (the upsert already knows which rows changed).

20. **P2** ✔ **Date ordering and filtering go through `json_extract` with no index.**
    - **Where:** `recent_messages` ([cache.rs:2015-2020](source/crates/mail/src/cache.rs#L2015-L2020)), `hour_histogram` ([1839](source/crates/mail/src/cache.rs#L1839)) and the `search_query` sort ([1957](source/crates/mail/src/cache.rs#L1957)).
    - **Code:** each runs `ORDER BY` / `WHERE json_extract(data,'$.date') …`, which is a full scan plus JSON parse plus sort, even for `LIMIT 20`.
    - **Fix:** a generated column `date TEXT GENERATED ALWAYS AS (json_extract(data,'$.date')) VIRTUAL` with an index on `(partial, date)`. SQLite supports indexed generated columns, so no data migration is needed.

21. **P3** ✔ **The contacts snapshot diff deep-clones the whole previous address book on every change.** [contacts_view.rs:2527-2537](source/crates/app/src/contacts_view.rs#L2527-L2537).
    - **Code:** `previously_seen: HashMap<String, VCard>` clones every previous card, photos included, to find the few that disappeared.
    - **Fix:** build a `HashSet` of identities from the new snapshot, iterate the previous records by reference, and clone only the missing ones.

22. **P2** ✔ **Graph consent checks read and parse the token file on the GTK thread.** [microsoft_oauth.rs:355-364](source/crates/app/src/microsoft_oauth.rs#L355-L364).
    - **Code:** every `consented()` (the seven `graph_*` wrappers) and `graph_mail_consented()` call does `TokenStore::load()`: a synchronous `fs::read` plus JSON parse of the refresh-token file.
    - **Call sites:** filter loops ([graph_sync.rs:93, 271, 359, 485](source/crates/app/src/window/graph_sync.rs#L485)), `focused_inbox_available` ([support.rs:657](source/crates/app/src/window/support.rs#L657)), the composer's directory assembly ([graph_directory.rs:84](source/crates/app/src/graph_directory.rs#L84)), Settings repaints, and ad-hoc `MicrosoftOAuth::new(..).graph_consented()` in [contacts_view.rs:2340, 3003, 3013](source/crates/app/src/contacts_view.rs#L3003) and [connect.rs:1986, 2425](source/crates/app/src/window/connect.rs#L1986).
    - **Fix:** cache the flags in memory on the shared per-account `MicrosoftOAuth` (BUG_BOUNTY1 #5's registry) and update them on save.

23. **P1** (a) / **P2** (b) ✔ **Draft autosave re-uploads every attachment on every changed tick.** [compose.rs:24, 1113-1137](source/crates/app/src/compose.rs#L1113-L1137).
    - **Code:** with a 5 s tick, each changed snapshot builds a `ComposedMessage` that deep-copies every attachment (`(*self.bytes).clone()`). The session then re-`APPEND`s the whole MIME message to Drafts, and deletes the previous copy.
    - **Cost:** typing in a composer with a 20 MB attachment uploads 20 MB every 5 seconds.
    - **Fix:**
      - Back off the interval when attachments exceed a threshold (e.g. 30 s or more), or save on idle/blur rather than on every tick.
      - Pass `Rc`/`Arc<[u8]>` into `Attachment` so the copy is shared.

24. **P1** (a) / **P4** (b) ✔ **Synchronous SQLite and file I/O on the GTK thread** (beyond BUG_BOUNTY1 #16 and #17).
    - **What:** `UiStateDb` writes (`set_starred`, `set_recent_folders`, `set_folder_expansion`, reminder state, My Day, local tasks: about 40 call sites) run on the main thread with the default `synchronous=FULL` (one fsync per write) and a 5 s `busy_timeout` ([ui_state_db.rs:147-156](source/crates/app/src/ui_state_db.rs#L147-L156)).
    - **Fix:** set `synchronous=NORMAL` (WAL makes that safe), and move bulk writers such as `set_recent_folders` and reminder sync off the main thread.
    - **Same pattern:** `icon_style::materialise` ([icon_style.rs:175-209](source/crates/app/src/icon_style.rs#L175-L209)) `remove_dir_all`s and rewrites the whole icon pack synchronously on every switch. GTK can also scan the half-written directory in between. Write it to a sibling directory and rename it into place.

25. **P2** ✔ **The Graph mail session opens its cache synchronously on the async runtime.** [graph/src/mail/session.rs:122](source/crates/graph/src/mail/session.rs#L122).
    - **Code:** `Cache::open` (file open, `PRAGMA`s, DDL, migrations) runs inline on the tokio worker. Everything else in the session goes through `cache_op` and `spawn_blocking`.
    - **Fix:** `spawn_blocking(move || Cache::open(..)).await`.

26. **P1** (weak ETags) / **P4** (rest) — **Minor:**
    - **P4** **Sort keys:** `sort_by_key(|c| c.display_label().to_lowercase())` allocates the key on every comparison ([contact_picker.rs:133](source/crates/app/src/contact_picker.rs#L133), [contacts_view.rs:1737, 3110](source/crates/app/src/contacts_view.rs#L1737)). These lists can hold thousands of contacts, so use `sort_by_cached_key`.
    - **P4** **Busy-wait:** `AutosaveCtx::flush` polls with a 50 ms `timeout_future` loop ([compose.rs:1081-1086](source/crates/app/src/compose.rs#L1081-L1086)). A `Notify`-style async signal would avoid the busy-wait.
    - **P4** **Zombie process:** [online_accounts.rs:59](source/crates/app/src/online_accounts.rs#L59) drops a spawned `gnome-control-center` `Child` without waiting, which leaves a zombie until Lookout exits. Use `gio::Subprocess` or `glib::spawn_async` with a child watch.
    - **P1** **Weak ETags:** `format!("\"{}\"", etag.trim_matches('"'))` ([dav/src/client.rs:731, 751](source/crates/dav/src/client.rs#L731)) turns a weak validator `W/"abc"` into `"W/"abc"`, so every edit or delete on such a server fails with 412. Pass a value that is already quoted, or starts with `W/`, through unchanged.

## Tier 5 — Latent hazards

27. **P2** ✔ **Edition-2021 `if let … = x.borrow()…` keeps the `Ref` alive across a callback** (BUG_BOUNTY1 #26, widened).
    - **Scope:** 86 sites in `app/src` match `if let … = <rc>.borrow()… {` with a body that calls out.
    - **Highest risk:** the sites that invoke *hooks* while `state` is borrowed:
      - `state.borrow().contacts_refresh` at [contact_merge_view.rs:611](source/crates/app/src/contact_merge_view.rs#L611)
      - `contacts_details_refresh` at [window.rs:7843, 7856, 7873](source/crates/app/src/window.rs#L7843)
      - `composer_identities_refresh` at [window.rs:10281](source/crates/app/src/window.rs#L10281)
      - `dashboard_refresh` at [calendar_glue.rs:626](source/crates/app/src/window/calendar_glue.rs#L626)
      - `apply_incoming_imip_reply` at [reading.rs:1329](source/crates/app/src/window/reading.rs#L1329)
    - **Status:** these hooks currently take only shared borrows of `state`, so nothing panics today. The first `state.borrow_mut()` added anywhere inside them becomes an abort in a GTK trampoline.
    - **Fix:** bind the clone with `let hook = …;` first. Alternatively, moving the workspace to edition 2024 shortens these temporaries' lifetimes and fixes all 86 at once.

28. **P4** ✔ **Orphaned doc comment.** [support.rs:514-527](source/crates/app/src/window/support.rs#L514-L527).
    - **Problem:** the "Opens a mail account's address-book cache…" doc meant for `spawn_cache_open` (line 610) is glued onto `spawn_calendar_cache_store`, so both functions are mis-documented.
    - **Fix:** move it to `spawn_cache_open`.

## Tier 6 — Duplicated code

29. **P3** **Date helpers.** `first_of_month` is defined 8 times and `next_month`-style helpers 3 or more:
    - [search_query.rs:662](source/crates/mail/src/search_query.rs#L662), [eds_mirror.rs:372](source/crates/app/src/eds_mirror.rs#L372), [calendar_view.rs:4844](source/crates/app/src/calendar_view.rs#L4844), [calendar_glue.rs:74](source/crates/app/src/window/calendar_glue.rs#L74)
    - dav [subscription.rs:225-249](source/crates/dav/src/subscription.rs#L225), [session.rs:813-924](source/crates/dav/src/session.rs#L813), [cache.rs:309](source/crates/dav/src/cache.rs#L309)
    - graph [calendar/session.rs:536-540](source/crates/graph/src/calendar/session.rs#L536)

    Move them into `lookout_core` (a `month` module).

30. **P3** **Graph JSON and URL helpers, copied per module:**
    - `encode_segment` ×4: mail, contacts, calendar and todo `client.rs`.
    - `non_empty_str` ×4 and `string_array` ×5: the four `graph/src/*/mod.rs` plus [graph_rules.rs:400](source/crates/app/src/graph_rules.rs#L400).

    Move them into `graph/src/json.rs` (or `client.rs`) as `pub(crate)`, and export `string_array` for the app.

31. **P1** **Atomic-write family.** There are six hand-rolled tmp+rename copies, each slightly different and each with the pid or fixed-name collision from BUG_BOUNTY1 #12:
    - [mail cache.rs:135](source/crates/mail/src/cache.rs#L135)
    - [eds_mirror.rs:543](source/crates/app/src/eds_mirror.rs#L543), the only one that `sync_all`s
    - [background.rs:199](source/crates/app/src/background.rs#L199)
    - `TokenStore::save` in [microsoft_oauth.rs:191](source/crates/app/src/microsoft_oauth.rs#L191) and [google_tasks.rs:90](source/crates/app/src/google_tasks.rs#L90)
    - `set_client_id` in [google_tasks.rs:268](source/crates/app/src/google_tasks.rs#L268)

    Three more writers are plain `fs::write`: `app_config::save_at`, `calendar_colors::save` and `google_tasks::store_cache`. Replace all nine with one `lookout_core::fsutil::write_atomic(path, bytes, mode)` that uses `create_new`, a unique suffix, `mode(0o600)` where needed, `sync_all` and rename.

32. **P2** **OAuth plumbing (Microsoft vs Google).** The two files differ only in the provider name:
    - `receive_auth_code`, `read_request_until_headers`, `respond_html`, `parse_query`, `open_browser`, `oauth_dir`, `pkce_verifier`/`pkce_challenge`, `post_token_request` and `TokenStore`
    - in [microsoft_oauth.rs:598-740](source/crates/app/src/microsoft_oauth.rs#L598-L740) and [google_tasks.rs:300, 814-910](source/crates/app/src/google_tasks.rs#L814-L910)
    - `diff` of `receive_auth_code` shows only the "Microsoft"/"Google" strings changing.

    Extract an `oauth_common.rs` (BUG_BOUNTY1 #30, still open), which also fixes #2 above in one place.

33. **P2** **Graph feature clients.** Seven `graph_*` wrappers each carry their own `MicrosoftOAuth`/`GraphClient` and an identical `consented()`, and four have an `ensure_consent()` that differs only in which consent call it makes:
    - [graph_categories.rs:49](source/crates/app/src/graph_categories.rs#L49), [graph_rules.rs:63](source/crates/app/src/graph_rules.rs#L63), [graph_pin.rs:88](source/crates/app/src/graph_pin.rs#L88), [graph_calendar_share.rs:42](source/crates/app/src/graph_calendar_share.rs#L42), [graph_automatic_replies.rs:47](source/crates/app/src/graph_automatic_replies.rs#L47), [graph_mail.rs:47](source/crates/app/src/graph_mail.rs#L47), [graph_directory.rs:74](source/crates/app/src/graph_directory.rs#L74)

    `MicrosoftOAuth::new` appears 21 times. One per-account `GraphAccount { oauth: Arc<MicrosoftOAuth>, graph: GraphClient }` shared by all features removes the duplication and is the registry BUG_BOUNTY1 #5 needs.

34. **P3** **IMAP vs Graph session actors.** They share `cache_op` (identical), the outbox drain (decode, delete undecodable, send, delete draft, delete row, retry and fail bookkeeping), and `outbox_retry_delay`/`tag_keys`. [session.rs:1216](source/crates/mail/src/session.rs#L1216) vs [graph/src/mail/session.rs:2281](source/crates/graph/src/mail/session.rs#L2281); [session.rs:1771-1880](source/crates/mail/src/session.rs#L1771) vs [graph/src/mail/session.rs:812-880](source/crates/graph/src/mail/session.rs#L812-L880).
    - **Fix:** move `cache_op` and an `OutboxDrainer` that takes a `send` closure into `lookout_mail`. The #6 fix then lands once.

35. **P4** **Small text helpers:**
    - `escape_like` ×2 ([search_query.rs:724](source/crates/mail/src/search_query.rs#L724), [dav cache.rs:394](source/crates/dav/src/cache.rs#L394))
    - `strip_html` ×2 ([body.rs:1057](source/crates/mail/src/body.rs#L1057), [dav cache.rs:471](source/crates/dav/src/cache.rs#L471))
    - `escape_text` ×3 ([body.rs:765](source/crates/mail/src/body.rs#L765), [freebusy.rs:282](source/crates/dav/src/freebusy.rs#L282), [xml.rs:521](source/crates/dav/src/xml.rs#L521))
    - `text_to_html` ×2 ([compose.rs:575](source/crates/app/src/compose.rs#L575), [graph_automatic_replies.rs:143](source/crates/app/src/graph_automatic_replies.rs#L143))
    - `strip_mailto` ×2 ([freebusy.rs:273](source/crates/dav/src/freebusy.rs#L273), [ical.rs:259](source/crates/dav/src/ical.rs#L259))
    - `cache_dir` ×3 and `sanitize_filename` ×3 (mail cache, dav cache, ui_state_db; dav client)

    Move them into `lookout_core::{text, fsutil}`.

36. **P3** **Test scaffolding.**
    - `fn block_on` (a fresh current-thread runtime) is copied into 7 test modules: graph_directory, eds_directory, ldap_directory, evolution_import, other_accounts, assistant, graph_rooms.
    - A fake `TokenProvider` (`graph_token`/`ensure_scopes`) is re-implemented 9 times across `graph/src/**` and `app/src/graph_{directory,rooms}.rs`.

    A `#[cfg(test)]` `test_support` module per crate (and a `pub` `StaticToken` in `lookout_graph` behind a `test-util` feature) removes them.

## 7. BUG_BOUNTY1 items re-verified

Rows 2 and 4 are kept for traceability because they were closed by work in this document (#8 and #15 respectively); the rest are still open.

| # | Priority | Item | State now |
|---|----------|------|-----------|
| 2 | P1 | Concurrent rule push across accounts | Addressed by #8 (0.9.298) |
| 4 | P1 | `settings.json` non-atomic write | Closed by #15 (0.9.299) |
| 5 | P2 | Per-instance `MicrosoftOAuth`, fixed `json.tmp`, stale consent flag on refresh | Open (21 `MicrosoftOAuth::new`, `with_extension("json.tmp")` at [microsoft_oauth.rs:197](source/crates/app/src/microsoft_oauth.rs#L197)) |
| 6 | P2 | Google OAuth: no refresh lock; `exists()` then `load()?` TOCTOU | Open ([google_tasks.rs:150-160](source/crates/app/src/google_tasks.rs#L150-L160)) |
| 7 | P3 | `sender_photo_cache` never cleared or capped | Open ([window.rs:863](source/crates/app/src/window.rs#L863), only 3 references) |
| 8 | P2 | ICS import duplicates UIDs outside loaded months | Open ([imip.rs:1153](source/crates/app/src/window/imip.rs#L1153)) |
| 9 | P1 | Token files written at umask then chmod | Open; folded into #31 |
| 10 | P1 | Attachment "Open" temp files at umask in shared `$TMPDIR` | Open ([attachments.rs:719-731](source/crates/app/src/window/attachments.rs#L719-L731)) |
| 12 | P1 | `write_atomic` pid-suffix collision | Open; see #31 |
| 16 | P3 | ICS/vCard import read synchronously, `file.path()` only | Open ([calendar_glue.rs:3208](source/crates/app/src/window/calendar_glue.rs#L3208), [contacts_view.rs:1050](source/crates/app/src/contacts_view.rs#L1050)) |
| 17 | P3 | Blocking contacts SQLite inside async tasks | Open; see #18 |
| 25 | P4 | `undo_stack.remove(0)` | Open ([mail_actions.rs:2772](source/crates/app/src/window/mail_actions.rs#L2772)) |
| 26 | P2 | Edition-2021 borrow temporaries | Open; see #27 |
| 27 | P2 | `lock().unwrap()` poisoning cascade | Open (85 in the mail cache, 12 in the dav cache) |
| 28 | P3 | `DefaultHasher` for on-disk names | Open ([cache.rs:120-124](source/crates/mail/src/cache.rs#L120-L124)); the doc comment calls it "fixed-seed", but the algorithm itself is not stable across Rust releases |
| 29–30 | P3 | Shared helpers / app duplicates | Open; expanded in Tier 6 |
| 31 | P4 | `#[allow(dead_code)]` items | Partly: [app_config.rs:39, 667](source/crates/app/src/app_config.rs#L39) and [message_list.rs:1150](source/crates/app/src/message_list.rs#L1150) remain |

Items 1, 3, 11, 13, 14, 15 and 18–22, 24 are fixed. Item 23 was not re-checked.

## Not covered
- The vendored `async-imap` and `imap-proto` crates (as in BUG_BOUNTY1).
- `core/src/interop/*` parsers (TNEF, DSN, CSV) were not profiled for pathological inputs.
- The IMAP session's IDLE supervisor and reconnect state machine were read only around the outbox and Sieve paths. A dedicated pass on `select`/IDLE interleaving is still worth doing.

## Suggested order

Tiers 1–3 and #4–#8, #11–#12, #14–#15 in Tier 4 are already fixed, so the list below covers the open work only, highest priority first. The P tags on each item follow the legend above.

**P1 — next: small diffs, no schema work**

1. **#26 (weak ETags):** one line, and it unblocks every edit or delete against a weak-validator DAV server.
2. **#24 (a):** `UiStateDb` `synchronous=NORMAL` plus the icon-pack sibling-dir rename — the two biggest GTK stalls in the tier.
3. **#17:** `ContactsCache::remove_book`, which stops an unbounded per-poll re-parse.
4. **#23 (a):** autosave interval backoff / save-on-idle, which removes the 20 MB-per-5-s upload without touching the `Attachment` type.
5. **#31:** the shared `fsutil::write_atomic`, which also closes BUG_BOUNTY1 #9 and #12.

**P2 — schedule next**

6. **#23 (b):** `Rc<[u8]>`/`Arc<[u8]>` attachments.
7. **#22 + BUG_BOUNTY1 #5/#6 + #32/#33:** one shared per-account OAuth/Graph account with cached consent flags, so the consent cache is written once.
8. **#25:** `spawn_blocking` around `Cache::open`.
9. **#20:** the generated `date` column and `(partial, date)` index.
10. **#27 + BUG_BOUNTY1 #26:** bind the hook clone before calling it (or move to edition 2024), which fixes all 86 sites at once.
11. **BUG_BOUNTY1 #8:** ICS import duplicating UIDs outside the loaded months.

**P3 — do with the area open**

12. **#19 (a):** the `flagged` column plus partial index; leave the "emit only on change" half (`#19 (b)`) until the upsert plumbing is already open.
13. **#18 + BUG_BOUNTY1 #17:** contacts cache pragmas, transactions and `spawn_blocking`.
14. **#21:** the snapshot diff without the deep clone.
15. **#29, #30, #34, #36:** helper and test-scaffolding de-duplication, folded in as those files change.
16. **BUG_BOUNTY1 #7, #16, #28:** the remaining small open items.

**P4 — opportunistic**

17. **#19 (b), #24 (b), #26 (rest), #28, #35**, plus BUG_BOUNTY1 #25 and #31.

## Verification
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings -A clippy::result_large_err`
- `dbus-run-session -- cargo test --workspace`
- **New tests worth adding:**
  - `managesieve`: `connect` to a fake server without `STARTTLS` returns an error and never sends `AUTHENTICATE`.
  - `receive_auth_code`: a request carrying `code` but no `state` is rejected.
  - `empty_mailbox`: against the fake IMAP harness, a UID appended after `SEARCH` survives.
  - Outbox: a fake transport that succeeds, then a panic injected before archive; the row is gone on restart.
  - `ContactsCache::remove_book`: removes the book and its members; a second sync finds nothing stale.
  - `GraphClient`: a `Retry-After: 3600` waits at most `MAX_RETRY_DELAY`; a 401 triggers exactly one invalidate-and-retry.
  - Rules sync: a remote-deleted rule with a Graph id is deleted locally and not re-created.
- **Manual (ask before launching; see memory):**
  - Double-click Send.
  - Close a rich-text composer within 5 s of typing and check Drafts.
  - Suspend for 2 hours with an M365 account, resume and check mail and Graph calls work immediately.
