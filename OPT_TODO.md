# TODO

## Optimisation roadmap

Performance audit of the IMAP pipeline, SQLite cache layer, and GTK message list. The dominant systemic cost: **every IDLE wake triggers a full-mailbox flag fetch, a whole-table DB rewrite, and a full-list UI repopulate** — all O(mailbox size) when only a few messages changed. The codebase already defers the fix ("Full CONDSTORE/QRESYNC incremental sync is Phase 2", `mail/src/session.rs:26`).

## Phase 1 — Quick wins

Completed 17 Aug 2026: `9268e0e` covered the redundant SELECT on IDLE wake, redundant post-sync STATUS, capability caching, SQLite pragmas (mail + DAV), `body_index_text` size-check-first, cached remote-content scan, and the vendored-parser round. A second pass (`90e5529`) then batched the per-UID SQLite N+1s (new `*_many`/`has_bodies` cache methods), replaced the per-hit FTS loads with one FTS→messages JOIN, and empirically disproved the `check_path` O(n²) claim (see below). **Phase 1 is now complete.**

- [x] Kill the redundant SELECT on every IDLE wake — `sync_mailbox` unconditionally re-selects the folder (`session.rs:2113`) even though `session_selected == current_mailbox_id` is guaranteed before IDLE re-entry (`session.rs:674-677`); skip it when the session is already on that mailbox — **done**: `sync_mailbox` takes `session_selected` and skips the SELECT when it matches (`session.rs:2243-2261`); SELECT/STATUS paths write `uidvalidity`/`uidnext` back so the skip path still keys the cache. Steady-state wake: 3 RTTs → 1
- [x] Kill the redundant STATUS after sync — the unread count for the woken folder is computable from the flags already fetched; drop `refresh_one_folder_count`'s extra round trip (`session.rs:728`) — **done**: counts derived from the flag fetch (`unread` from flags, `total` from fetch size, `uidnext`/`uidvalidity` from SELECT meta), folder updated in place, sidebar republished only on change
- [x] Cache capabilities — `session.capabilities()` issues a live `CAPABILITY` command on **every** MOVE batch (`session.rs:1744`, async-imap `client.rs:674-683`); fetch once after login, store `has MOVE` in session state — **done**: post-login fetch at `session.rs:2048-2050`, threaded through the move paths
- [x] Batch the per-UID SQLite N+1s — **done** via new batch methods on `Cache`, all one-transaction with prepared-statement reuse:
  - `has_body` per UID in the prefetch envelope pass (`session.rs:1577-1579`) → `has_bodies` answers for the whole envelope batch with one `SELECT uid FROM bodies WHERE mailbox_id=? AND uidvalidity=?`, filtered to the wanted set (`cache.rs`)
  - `delete_message` per UID in move handlers → `delete_messages(mailbox, &uids)` drops envelopes + bodies + snooze + FTS rows in one transaction (prepared-statement reuse per uid instead of `IN (...)`, safe against SQLite's variable-count limit on huge batches)
  - `update_flags`/`update_keywords` per UID per batch → `update_flags_many`/`update_keywords_many`, same all-or-nothing contract (any uid outside the cached window returns `false` and the session resyncs)
  - `snooze_message` per UID → `snooze_messages(mailbox, &uids, until)` in one transaction
- [x] Fix `replace_messages`' FTS side effect on search results — the search path loaded each hit with its own SELECT + JSON parse (`cache.rs:897-906`, up to 300 per keystroke on the UI thread) → **done**: one `search_fts JOIN messages` query fetches every hit's envelope in a single statement; hits whose envelope row is gone fall out of the JOIN instead of being skipped one by one
- [x] SQLite pragmas: `synchronous=NORMAL` on the mail cache (disposable data, currently FULL = fsync per autocommit commit) and the DAV cache (which also lacks WAL/busy_timeout, `dav/src/cache.rs:110-127`) — **done**: mail cache `synchronous=NORMAL` (`cache.rs:305-315`), DAV cache WAL + 5s busy_timeout + NORMAL (`dav/src/cache.rs:120-122`)
- [x] `body_index_text` strips the full HTML before the size check (`cache.rs:194-210`) → check `FULL_BODY_INDEX_BYTES` first — **done**: plain-text part checked against the limit first, HTML strip skipped when the text part alone is already over (`cache.rs:199-213`)
- [x] Cache the remote-content scan per body (`window.rs:13462`) — re-scans the whole HTML on every render/selection — **done**: single-slot `UiState` cache keyed by `(mailbox, uid)`, reused across re-renders (`window.rs:676`, `13363-13374`)
- [ ] Vendored-parser fixes:
  - [x] `section_part` uses `Vec::insert(0, …)` → O(k²) per part path (`imap-proto/src/parser/rfc3501/body.rs:16-20`) → `once(part).chain(rest)` — **done** (`body.rs:20`)
  - [x] `rfc5464.rs` O(n²) `check_path` rescan per path component (`rfc5464.rs:71-83`) → single linear scan — **verified, no change needed**: the claim was wrong. The state machine advances monotonically (every `Path(l+j)` offset is strictly greater than `l`, `j ≥ 1`), so each byte is examined at most twice. Measured empirically: 29 / 389 / 4 889 / 58 889 / 688 889 comparisons for 10 / 100 / 1k / 10k / 100k components (≈ len − 14 each) — a linear single pass, not quadratic
  - [x] `rfc5464.rs` String churn + `slice_to_str().unwrap()` panics (`rfc5464.rs:124-150`) → `Cow<'a, str>` + `map_res(from_utf8)` — **done**: `slice_to_str().unwrap()` (a non-UTF-8 METADATA value crashed the connection) replaced by `to_utf8` with a `Verify` parse error, `entry_list` emits `Cow::Borrowed` directly
  - [x] `rfc2971.rs`/`rfc4314.rs` use `nom::character::complete::*` in a streaming pipeline (`rfc2971.rs:14-15`, `rfc4314.rs:15-17`) — a response split across TCP segments kills the connection instead of waiting → switch to `streaming` variants — **done**: both parsers now import `character::streaming::*`
  - [x] Command builder `format!`+`into_bytes()` double work and `to_string()` temps per sequence number (`builders/command.rs:30,214-227`) → single-pass `Vec<u8>` with `itoa`-style formatting — **done**: `push_quoted` + stack-buffered decimal formatter write straight into the output buffer

## Phase 2 — Incremental sync (the big one)

- [ ] Enable CONDSTORE — `ENABLE CONDSTORE` after login (async-imap `select_condstore`/`run_command`), persist per-mailbox `highest_modseq` (field already exists in `core/src/mailbox.rs:32`, never populated — `session.rs:1915`, `2567`, `window.rs:13891` hardcode `None`)
- [ ] Delta flag fetch — replace `FETCH 1:* (UID FLAGS)` (`session.rs:2129`) with `FETCH 1:* (UID FLAGS) (CHANGEDSINCE <modseq>)`; steady-state cost drops from O(mailbox) to O(changed)
- [ ] QRESYNC — `ENABLE QRESYNC` + `SELECT ... (QRESYNC)` reports expunged UIDs too (currently expunges are only discovered by UID-set diff)
- [ ] Diff-based cache writes — `replace_messages` (`cache.rs:505-523`) does `DELETE FROM messages` + `DELETE FROM search_fts` + full re-INSERT + re-serialization + full FTS re-index on **every** sync (`session.rs:723-724`, 746, 792, 1232, 1273, 1339, 1417); instead upsert only new/changed UIDs (`INSERT ... ON CONFLICT(mailbox_id, uid) DO UPDATE`), delete only UIDs absent from the fetch, re-index FTS only for those
- [ ] Skip `compute_thread_keys` when the UID set and message-ids are unchanged since last sync (`session.rs:2199-2204`)
- [ ] Run cache I/O via `tokio::task::spawn_blocking` — zero `spawn_blocking` exists in the crate today; a heavy cache write stalls the account's actor future and holds the shared worker (several accounts → worker starvation)
- [ ] Move `backfill_search_index` (`session.rs:495`) off the session-critical startup path — currently re-parses every cached row + body on the async worker before the connection is even attempted
- [ ] Purge `bodies` rows + orphaned flat files for expunged/replaced-away messages (see Phase 5), and drop the never-pruned `addresses` table growth (`cache.rs:743-768`)

## Phase 3 — Off-main-thread DB

- [ ] Route every UI-side cache read through the worker with bounded reply channels (pattern already exists in `contacts_view.rs:296`):
  - Full-mailbox load + JSON deserialize on folder switch (`window.rs:11389`, `11616`)
  - FTS search + per-hit loads per keystroke (`window.rs:11486-11499`)
  - Composer autocomplete LIKE query per keystroke (`window.rs:13814`)
  - `hour_histogram(None)` full-table scan with `json_extract` per row (`window.rs:9714-9727`, `cache.rs:837-858`) — also debounce + only when the dashboard is visible
  - `Cache::open` schema DDL at account connect (`window.rs:7490`)
- [ ] Make UI-side SQLite access read-only — `active_snoozed_uids` runs a `DELETE` on the UI thread (`window.rs:11390`, `cache.rs:1011`) that can block up to the 5s `busy_timeout` behind the session's write transaction; move the cleanup to the session thread
- [ ] Debounce dashboard refreshes (500ms trailing edge, pattern exists at `window.rs:4242`) — currently `refresh_lookout_view` re-scans every account's whole message table on **every** `FoldersUpdated`/`MessagesUpdated` (`window.rs:7577`, `7598`)

## Phase 4 — Round-trip reduction

- [ ] Batch the background body prefetch — 1 `uid_fetch` round trip per message, 10 per batch (`session.rs:1612-1638`) → one `UID FETCH <joined set> (BODY.PEEK[HEADER] BODY.PEEK[<union of parts>])` per batch; keep the yield-to-commands check between batches, not messages
- [ ] LIST-EXTENDED counts — `LIST "" "*" RETURN (STATUS (MESSAGES UNSEEN UIDNEXT UIDVALIDITY))` in one round trip instead of the per-folder STATUS drain (`session.rs:1480-1525`, ~1 RTT per folder × 100); fall back to the drain when the server rejects it. Also populates `uidvalidity`/`uidnext` for every folder for free
- [ ] Chunk large joined UID sets (`join_uids`, `session.rs:1676`, and ad-hoc joins at 2153/2086/2342/1806/1823) — first sync of a big folder can exceed server line limits (Dovecot 64 KB) and tear the connection down; chunk to ~1–5k UIDs or `a:b` ranges, use `1:*` when covering the whole folder
- [ ] Targeted count refreshes after moves/sends — `relist_folders` re-lists all folders and re-queues a STATUS for **every** folder after each move/send/draft-create (`session.rs:999`, `1035`, `1082`, `1120`, `1167`, `1204`, `1965-1980`); only the source + target counts changed
- [ ] Draft replace with `UID EXPUNGE` — `purge_by_message_id` is SELECT + `UID SEARCH HEADER` + STORE `\Deleted` + EXPUNGE + APPEND ≈ 5 RTTs per autosave (`session.rs:1818-1827`, `1838-1859`); `uid_expunge` cuts the STORE+EXPUNGE pair
- [ ] COMPRESS=DEFLATE after login (`run_command("COMPRESS DEFLATE")`) — 4–10× transfer cut on the first sync of a large folder and the per-wake flag fetch
- [ ] Coalesce pending `FetchBody` commands for the same mailbox (`session.rs:797-830`) — paging quickly through uncached mail costs one RTT per message

## Phase 5 — Incremental UI

- [ ] Allocation-free no-op check in `repopulate` (`message_list.rs:775-928`) — the unchanged-check currently deep-clones the whole mailbox into `truth` (`:778`), O(n log n) sorts, then computes `message_row_key` (≈5 heap allocs × n × 2 sides, `:797-804`) before bailing; precompute a `Vec<MessageRowKey>` and compare that instead
- [ ] Skip the second full clone — `displayed` gets a second deep clone of the set (`message_list.rs:827`) even when the no-op check is about to return
- [ ] Incremental splices — real changes currently rebuild the entire tree: `capture_collapsed` + full splice + `apply_expansion` materializes every row + `restore_selection` walks the flat model (`message_list.rs:1022-1163`); patch ranges via `ListStore::splice`, maintain collapse sets incrementally, restore selection from the sorted key vector
- [ ] Move the sort/group off the main thread (or make `repopulate` async over `spawn_blocking`)
- [ ] Bound + coalesce the event channels — all account/session channels are `async_channel::unbounded()` (`window.rs:7474-7475`, `8028-8029`, `8527-8528`, `9068-9069`, `9196-9197`) with no backpressure; the UI drains serially, one full `repopulate` per event → `bounded()` + drain with `try_recv` and collapse repeated `MessagesUpdated`/`FoldersUpdated` into the last one (pattern exists in `contacts_view.rs:1376`). Turns the startup burst from N repopulates into 1
- [ ] Stop double-emitting the full mailbox per sync — `fetch_previews` emits a second full `MessagesUpdated` (`session.rs:2363`) differing only in previews; fold previews into the first emit or send a delta
- [ ] Incremental unified-view merge — each inbox event re-merges and re-sorts the whole unified set with deep clones (`window.rs:7663-7668`, `11655-11667`); keep one merged sorted vec and merge-slice only the changed account
- [ ] Flat-file hygiene: `delete_message` never removes the attachment `.bin`/`.eml` sidecars (`cache.rs:981-990`) → orphans accumulate on every move; write-to-temp + rename for atomicity (`cache.rs:664-671`, `700-707`); opportunistic sweep of files whose `(mailbox, uid, uidvalidity)` has no `messages` row; periodic `PRAGMA incremental_vacuum`

## Notes

- The per-response tail copy in the response stream (`async-imap-0.11.3/src/imap_stream.rs:101-143, 287-316`) is O(k²) memcpy + 2 allocations + 4 KB memset across a multi-response read (exactly what full-folder `FETCH` bursts produce). Fixing it requires vendoring/patching `async-imap` the way `imap-proto` is already patched (see `Cargo.toml` `[patch.crates-io]`): parse small responses into `Response::into_owned()` and `BytesMut::advance()` the consumed prefix instead of copying the tail.
- Things deliberately preserved: batched preview fetch (50/`UID FETCH` with `BODY.PEEK[]<0.16384>`, `session.rs:2342-2344`), partial-fetch body loading, `BODY.PEEK` everywhere (`\Seen` never set implicitly), `.SILENT` stores, joined MOVE/STORE commands, envelope reuse for cached UIDs, 25-min IDLE slicing, cooperative prefetch/STATUS drains.
