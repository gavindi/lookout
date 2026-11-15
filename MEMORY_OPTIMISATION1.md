# Lookout memory growth: findings and fix plan

## Context

The user sees the release build using over 1 GB of RAM. Measurements of the running process (pid 25095, up 4h45m):

| Counter | Value |
|---|---|
| RSS | 5.16 GB, all anonymous |
| Swap | 1.69 GB |
| Peak RSS (`VmHWM`) | 6.4 GB |
| Main-thread glibc heap (`[heap]`) | 4.58 GB resident + 1.62 GB swapped |
| Thread arenas | ~0.4 GB (small) |
| WebKit web process | 110 MB (not the problem) |
| Growth | ~25 MB in a few minutes; ~1.3 GB/hour on average |

So this is a steady leak on the **GTK main thread**, not a one-off load and not WebKit.

The code was read, not profiled: `ptrace_scope=1` blocks reading the heap without sudo, and the app was not relaunched. Three independent passes over the code (UI caches, mail data and channels, rendering and widgets) agree on the main cause. I checked the top items myself in the code.

## Findings, ranked by likely impact

### 1. Folder sidebar rows are never freed (main suspect, confirmed in code)
- **Cause:** each row's setup callback (`crates/app/src/window.rs:203-363`) creates reference cycles. A cycle keeps the row's widget tree alive after GTK unparents it, so it is never disposed:
  - `window.rs:257-270`: the `DropTarget` that the expander owns has enter/leave closures that capture `expander.clone()`.
  - `folder_menu.rs:211-219`: `dispatch_folder` captures the menu `button`, while the button owns the action group (`context_menu.rs:281`) that calls `dispatch_folder`.
  - `folder_menu.rs:304-311`: each of the 10 colour swatches captures its own ancestor `menu` popover.
  - `window.rs:342-345`: the popover's `closed` handler holds `refresh_actions`, which holds the button and the action box.
  - The folder factory has no unbind or teardown handler.
- **Trigger:** `rebuild_folder_tree` (`window/folders.rs:248-254`) builds a new `TreeListModel` and calls `set_model` whenever `folder_tree_signature` changes. The signature includes every folder's `unread` and `total` (`folders.rs:27`), so any count change rebuilds the tree: new mail, mark read, a move, a Drafts autosave, `FoldersUpdated` after each STATUS batch, or a Graph 45 s poll.
- **Cost:** each row carries a full `PopoverMenu` (about 18 items, 2 submenus, 10 swatches, 13 actions), roughly 100-250 KB. With 30-60 visible rows, each rebuild leaks about 3-15 MB. That fits ~1.3 GB/hour.
- **Probe:** `folder_row_spinners` (`state.rs:1757`) keeps weak references to the spinners and prunes only dead ones. If rows leak, the number of live weak references per mailbox grows on every rebuild.

### 2. Calendar sidebar row menus leak on every rebuild
- **Cause:** in `calendar_view.rs:4693-4702`, each swatch captures its own ancestor `menu`.
- **Trigger:** `rebuild_calendar_checklist` runs on every `ConnectionStateChanged` (`connect.rs:2912`; the sessions send Idle and Busy on each poll), on `CalendarsUpdated` (`:2918`) and on webcal `SubscriptionsUpdated` (`:3146`). There is no check for whether anything changed.

### 3. Tasks view rows leak on every task refresh
- **Causes:**
  - `tasks_view.rs:917-951` (`attach_my_day_menu`): the row's own `GestureClick` closure captures `row.clone()` and a `CalendarTask` clone.
  - `set_nav` (`tasks_view.rs:574-599`): the rename/delete buttons capture their ancestor popover.
- **Trigger:** `refresh_tasks_view` (`calendar_glue.rs:2548`) runs on every Google, Graph or CalDAV task poll (CalDAV runs it twice: once from cache, once from the network), even when nothing changed.

### 4. Parts of each message-list row leak when GTK destroys it
- **Causes:**
  - `profile_card.rs:234-257` (`PersonHover::add_trigger`): a motion controller owned by the widget holds a strong `widget.clone()`. It is used on the avatar, sender label and `single_sender` (`window.rs:1616-1622`).
  - `window.rs:1389-1397`: `thread_single_toggle` captures itself.
- **Trigger:** `splice_diff` (`message_list.rs:1119`) replaces every row whose key changed (read, flag, preview patch, folder switch).
- **Side effect:** `row_display_registry` (`window.rs:894`, `:1529`) is pruned only when a display setting changes, and the leaked rows keep their entries alive.

### 5. Unbounded Rust collections
- **`MessageListModel.threads`** (`message_list.rs:1271`): `clear_threads` (`:2054-2071`) empties each store and calls `keys.clear()`, but never removes the entry. The map grows with every conversation ever displayed. Every rebuild also iterates and clones the ids of all dead threads.
- **`sender_photo_cache`** (`window.rs:899`, `state.rs:502-512`): never evicted. It holds full-resolution decoded textures (~1-1.6 MB each), and each miss deep-clones the whole contact record (`Rc::unwrap_or_clone`).
- **`pending_optimistic_*` maps** (`optimistic.rs:192`, `:242-272`): never cleared if the confirming sync never arrives. These are small.

### 6. Leaks driven by user actions
- **Composer** (`compose.rs:1293`, `:1355`, `:2629-2639`): the WebView's own handlers capture it (via `AutosaveCtx`), and cc/bcc buttons capture themselves. Every closed composer leaks its WebView and attachment bytes.
- **Pop-out message window** (`message_window.rs:336-343`): the load handler captures the entry, which owns the window, web view and `last_raw` raw bytes.
- **Attachment strip** (`window/attachments.rs:131-206`): the menu items capture the menu button. Leaks once per attachment per message viewed.
- **Category popover** (`mail_actions.rs:1347-1390`): rows capture themselves and `Vec<EmailSummary>` of the selection.
- **Event peek popover** (`calendar_glue.rs:1640`): parented and never unparented. Conversation attachment menu (`conversation.rs:766-770`).

### 7. Duplicate copies and churn (bounded, but they fragment glibc's main heap)
Per message in the shown folder, the main thread holds:
- `truth: Vec<EmailSummary>`
- an `Rc::new(m.clone())` in every row object
- 2-3 copies of `MessageRowKey`, about 1 KB each, which copy subject, sender, preview and tags

Total: about 9 KB per message (~450 MB for a 50k folder).

Every `repopulate` adds temporary full copies:
- `truth.borrow().clone()` (`message_list.rs:1722`)
- `group_threads` clones every message (`:812`)
- `apply_layout` builds a row for every message but splices in only the changed range
- `patch_previews` (`optimistic.rs:444`) and `all_messages()` (26 call sites) clone again

**Merge and repurpose candidates in `EmailSummary` (`crates/core/src/email.rs:145`):**

| Field | Problem | Change |
|---|---|---|
| `structure: Option<Vec<BodyPart>>` | 0.8-5 KB; only needed when a message is opened | Load on open; `has_attachment` is already derived |
| `references: Vec<String>` | Always empty (IMAP and Graph) | Remove |
| `mailbox: MailboxId(String)` | Same ~80-180 B string on every message | Share as `Arc<str>` |
| `thread_key` | Usually duplicates the root `message_id` | Share |
| `from`, `to`, `cc` | Repeated addresses | Share |
| `flags: BTreeSet<SystemFlagBit>` | Five values | A `u8` bitmask |
| `preview` | Up to 500 characters; a row shows far fewer | Cap |

**Other duplicates:**
- `unified_snapshots` copies every inbox again in All Inboxes (`connect.rs:754`)
- flagged mail is held 3 times
- contact photos are held 3 times: `contacts_by_account`, category lists (`contacts_view.rs:1615`), birthdays (`calendar_glue.rs:310-316`)

**Allocator:** there is no `malloc_trim`, `mallopt` or alternative allocator anywhere.

### 8. Checked and fine
- `BodyCache` is an LRU of 25 entries.
- SQLite uses the default ~2 MB page cache and one connection per account per side.
- FTS5 search lives on disk.
- Prefetch writes bodies to disk and does not keep them in memory.
- The reading pane and conversation WebViews are reused singletons.
- Logging goes to stderr only.
- Graph and DAV snapshots replace the old data rather than appending.

### Side finding (disk, not RAM)
`~/.cache/lookout/mail` holds **22,656 `_test_*` caches (3.7 GB)** left by test runs. `cache_dir()` (`crates/mail/src/cache.rs:134`) uses the real `XDG_CACHE_HOME`, and tests that panic or skip their cleanup leave files behind.

## Fix plan

### Phase 0: confirm before changing code (user runs, or with explicit permission to launch)
1. **Leaked vs. merely retained:** on the running instance, run `sudo gdb -p <pid> -batch -ex 'call (int)malloc_trim(0)'` and compare RSS before and after.
   - A large drop means free-but-retained memory (fragmentation, finding 7).
   - A small drop means live leaked objects (findings 1-6).
2. **Leaked widgets:** run with `GOBJECT_DEBUG=instance-count GTK_DEBUG=interactive`. In Inspector → Statistics, `GtkPopoverMenu`, `GtkModelButton` and `GtkTreeExpander` counts should climb on each unread change if finding 1 is right.

### Phase 1: break the widget reference cycles (biggest win, low risk)
Pattern: in closures attached to a widget or its descendants, capture `downgrade()` / `glib::clone!(#[weak] x)` instead of strong clones, or use the signal's own widget argument (`gesture.widget()`, `controller.widget()`).

**Sites:**
- `window.rs:257-270`, `:342-345`, `:1389-1397`, `:1616-1622` (via `profile_card.rs:234-257`; also `attach_person_trigger` `:122-174`)
- `folder_menu.rs:211-219`, `:304-311`
- `calendar_view.rs:4693-4702`, `:887-893`
- `tasks_view.rs:917-951`, `:574-599`
- `window/attachments.rs:131-206`
- `mail_actions.rs:1347-1390`
- `compose.rs:1293`, `:1355`, `:1863-1878`, `:2629-2639`
- `message_window.rs:336-357`
- `conversation.rs:766-770`
- `calendar_glue.rs:1640` (unparent on close, as `tasks_view.rs:950` already does)

Grep the same shape elsewhere: `let X = X.clone();` followed by `X.connect_*` / `add_controller` on X or one of its ancestors.

### Phase 2: stop rebuilding when nothing visible changed
- **Folder tree:** keep the signature for structure only (ids, names, roles, delimiters, prefs, favorites). When only counts change, update the bound rows' badges in place (for example a per-mailbox count registry, like `folder_row_spinners`) instead of calling `set_model`.
- **Folder row menu:** build the "⋯" `PopoverMenu` lazily when the button is first activated (`MenuButton::set_create_popup_func`), not once per row.
- **Calendar checklist** (`connect.rs:2912`) and **tasks** (`refresh_tasks_view`): skip the rebuild when the input is unchanged, as desktop contacts already do. Rebuild on `ConnectionStateChanged` only when the status text changes.

### Phase 3: bound the Rust collections
- `MessageListModel::clear_threads`: `retain` only live ids. Also release capacity on the cleared `keys` (assign `Vec::new()`).
- `sender_photo_cache`:
  - scale photos to avatar size (for example 64-96 px) before caching
  - clear the cache on contacts snapshots
  - cap it with an LRU, like `BodyCache` (`state.rs:1136`)
  - borrow the contact instead of `Rc::unwrap_or_clone`
- `row_display_registry`: prune dead weak refs on insert.

### Phase 4: cut duplicate copies (medium effort, touches core types)
- `MessageListModel.truth` becomes `Vec<Rc<EmailSummary>>`. Rows share those `Rc`s instead of `Rc::new(m.clone())`. `all_messages()` and `patch_previews` stop deep-cloning.
- `apply_layout` builds row objects only for the spliced range.
- `EmailSummary`:
  - drop `references`
  - move `structure` out of the list path (load from the cache on open)
  - `mailbox` / `thread_key` become `Arc<str>`
  - this needs cache serialization care: `messages.data` JSON, `ENVELOPE_CACHE_VERSION`
- Optional: call `malloc_trim(0)` from an idle callback after a large repopulate, or set `M_MMAP_THRESHOLD` / `M_ARENA_MAX` at startup, depending on what Phase 0 shows.

### Phase 5 (separate, small): stop tests polluting the real cache
Point mail, DAV and app tests at a temporary `XDG_CACHE_HOME` (or a `cache_dir` override under `cfg(test)`). The user can delete the 22,656 leftover `_test_*` files by hand.

## Verification
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings -A clippy::result_large_err`.
- Tests under `dbus-run-session` (see memory note), Broadway for GTK tests.
- Add GTK tests that rebuild the folder tree, calendar checklist and tasks list, then assert that a `WeakRef` to an old row widget, its menu popover and a swatch upgrades to `None` after the rebuild (and after `glib::MainContext` iterations).
- Unit test: after `clear_threads` with an empty live set, `threads` is empty.
- In the running app (only with the user's go-ahead to launch): Inspector instance counts stay flat across several unread changes and 5-minute polls, and RSS stays roughly flat over an hour, instead of +1.3 GB.
- Version bump, CHANGELOG and metainfo entry per the repo's usual release-commit pattern.
