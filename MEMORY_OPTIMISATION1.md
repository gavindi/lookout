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

## Status (2026-11-15)

| Phase | State | Release |
|---|---|---|
| 0: measure | Done: a real leak, mostly the calendar sidebar | measured on 0.9.315 |
| 1: break widget reference cycles | Done, event and task editors included; not yet run in the real app | 0.9.316 (memory steady at ~600 MB, user report); composer in 0.9.318; editors in 0.9.319 |
| 2: stop rebuilding when nothing changed | Not started | |
| 3: bound the Rust collections | Done; not yet run in the real app | 0.9.317 |
| 4: cut duplicate copies | Not started | |
| 5: stop tests polluting the real cache | Code not started. This machine's leftovers were deleted on 2026-11-15: 6,735 entries, 1.2 GB; `~/.cache/lookout` went from ~2.1 GB to 901 MB | |

**Next step:** Phase 4 is where most of the remaining memory is. Phase 2 is smaller and mostly saves CPU now that rebuilt rows are freed.

**Still worth checking in the running app:** the Phase 0 probe's per-type counts across a session that opens and closes composers and editors. Do `AdwWindow`, `WebKitWebView` and `GtkPopoverMenu` stay flat? Under Wayland, is `GtkText` retained by the input-method module the way it is under Broadway?

**Open side item:** this machine's clippy (1.92) flags `nonminimal_bool` at `crates/mail/src/session.rs:274`, which is older than this work. Until it is fixed, the clippy command needs `-A clippy::nonminimal_bool`.

**Line numbers:** those in "Findings" and in the Phase 1 site list are from 0.9.315, before the fixes, and no longer match the code. For example, the folder row setup at `window.rs:203-363` is now `build_folder_row`.

## Phase 0 results (measured 2026-11-15)

**Setup:** installed `/usr/bin/lookout` 0.9.315, on real accounts, run for 51 minutes with an `LD_PRELOAD` probe.
- The probe wraps `g_object_new`, `g_object_new_valist` and `g_object_new_with_properties`, and adds a weak ref to each new object so its live count drops when the object is disposed.
- Every 60 s it logs RSS, `mallinfo2` and live/created counts per type.
- `GOBJECT_DEBUG=instance-count` does nothing here, because Ubuntu's GLib is built without `G_ENABLE_DEBUG`.
- The mail folder list was never on screen in this run (0 `GtkTreeExpander`s created), so finding 1 was not exercised.

### Growth from 10:49 to 11:40

| Counter | Start | End | Change |
|---|---|---|---|
| RSS | 808 MB | 1,653 MB | **+844 MB (~1 GB/hour)** |
| malloc in use | 497 MB | 1,251 MB | +754 MB |
| malloc free (kept by glibc) | 36 MB | 82 MB | +46 MB |
| `GtkPopoverMenu` live/created | 305/305 | 2,398/2,398 | **+2,093, none ever freed** |
| `GtkMenuButton` live/created | 43/320 | 43/2,443 | buttons rebuilt and freed |
| `GtkModelButton` live | | | +23,023 (~11 per leaked menu) |
| `GtkButton` live | | | +21,050 (10 swatches per leaked menu) |
| `GtkBox` live | | | +21,020 |
| `GMenu` live | | | +20,930 |
| `GSimpleAction` live | | | +16,744 |
| `GSimpleActionGroup` live | | | +2,093 (one per leaked menu) |
| `GtkWidget` live | 21,167 | 107,460 | +86,293 |
| `GObject` live | 79,343 | 314,653 | +235,310 |

**Flat over the run:** `GtkPicture`, `GdkTexture`, `GtkListItem`, `GtkListBoxRow`, `WebKitWebView` (3), `GtkLabel` (+120), `GtkCheckButton` and `GtkGestureClick` (+60 each; probably the tasks rows, finding 3).

**`malloc_trim(0)` at the end:** RSS went from 1,653 MB to 1,584 MB (−69 MB, about 4%). The growth is live leaked objects, not fragmentation, so allocator tuning would not fix it.

**Conclusion:**
- During this run the calendar sidebar (finding 2) accounts for nearly all of the growth.
- `rebuild_calendar_checklist` rebuilds one `MenuButton` + `ContextMenu` per calendar (~40 here) several times per 5-minute poll.
- Each popover leaks through the swatch → `menu` capture, at about 360 KB each (754 MB / 2,093).
- Leaks arrive in bursts of 161-276 menus (+58 to +100 MB) about every 5 minutes.
- The folder sidebar (finding 1) is not measured yet and would add to this whenever the mail view is visible.
- So Phase 1 should start with `calendar_view.rs:4693-4702`, then the folder row menu, which has the same swatch pattern (`folder_menu.rs:304-311`).

**Another test leftover:** `~/.cache/lookout/calendar` also holds 1,165 `eds-mirror-test-*` caches.

## Phase 1 results (2026-11-15, released as 0.9.316)

**In the running app (0.9.316, user report):** memory stays steady at about 600 MB, instead of the ~1 GB/hour growth Phase 0 measured. Popover instance counts were not measured.

**Fixed (handler no longer holds its own widget or an ancestor):**
- Calendar row menu swatches (finding 2) and folder row menu (finding 1): swatches hold the popover weakly; `dispatch_folder` holds the button weakly. `ContextMenu::popdown` was removed, because its only callers were these leaking closures.
- Folder row (`window.rs`): drop-target enter/leave use `target.widget()`; `refresh_actions` holds the action box and button weakly. The factory's setup body moved unchanged into `build_folder_row` so it can be tested.
- Tasks (finding 3): `set_nav` Rename/Delete, the `attach_my_day_menu` gesture (uses `gesture.widget()`, keeps only the task's key) and its item.
- Message rows (finding 4): `PersonHover::add_trigger` and `attach_person_trigger` use the controller's widget; `thread_single_toggle` uses the signal's button.
- Finding 6: attachment strip items (weak button); category menu rows (drop target and Manage button); conversation attachment menu; event peek (buttons, plus unparent on close); pop-out message window (every handler inside it holds the entry weakly; the close button and Esc find the window from the widget).
- Per-open popovers: the flag menu, the time-preset menu (Schedule send, Snooze) and `time_cluster`'s date popover.
- Composer, partly: cc/bcc buttons, the From dropdown hint, and the baseline `load_changed` handler, which now disconnects itself after it fires. `compose.rs:1293`/`:1355` were not cycles: the cloned `web_view` is only the receiver.
- New helper `util::popdown_enclosing_popover(widget)` closes the popover from the clicked item itself.

**Tests:** `gtk_test::assert_freed` plus 6 GTK tests:
- `checklist_rebuild_frees_the_previous_rows_and_menus`
- `dropped_row_menu_is_freed`
- `dropped_folder_row_is_freed`
- `nav_rebuild_frees_the_previous_rows_and_menus`
- `my_day_row_is_freed_when_dropped`
- `profile_card_triggers_do_not_keep_their_row_alive`

With the old swatch capture put back, the checklist test fails with exactly the Phase 0 signature: the `GtkPopoverMenu`s and their 10 `GtkButton` swatches stay alive while the `MenuButton`s are freed. The folder-row test fails likewise with the old `dispatch_folder`.

**Left open:**
- The composer may still leak as a whole: `AutosaveCtx` holds ~20 composer widgets and is captured by many handlers inside it. This needs an instance-count check in the running app, or a teardown that breaks the handlers.
- The event editor (`recurrence.rs`, `room_finder.rs`) was not reviewed.
- The ribbon's More / Move to / Quick steps popovers hold themselves too, but they are singletons whose rebuilt contents free normally, so they were left alone.

## Event and task editor results (2026-11-15, released as 0.9.319, not yet run in the real app)

**Measured first:** new tests open each editor, close it with Cancel and assert that every widget in it is freed.
- `event_editor::tests::closed_event_editor_is_freed`: a new event.
- `closed_editor_for_an_existing_meeting_with_scheduling_is_freed`: a recurring meeting with the Scheduling Assistant and the room finder.
- `task_editor::tests::closed_task_editor_is_freed`.

Before the fix, the whole event editor survived (about 2,000 widgets, `AdwWindow` included), and so did the task editor. GTK's `gtk_window_destroy` only drops the window's own reference, so a single handler that holds the dialog (Cancel's `dialog.close()`) keeps the whole window alive.

**Fixed:**
- **`util::HandlerSet`:** the composer's handler tracker, made shared. It adds `on_teardown` clean-ups for app-level callback slots, and `track` drops entries whose objects are gone. Both editors record their handlers in one and run it on the window's `close-request`. The editors have no other `close-request` handlers.
- **Helpers that connect on the editor's widgets** take the editor's set, so their handlers are disconnected with the editor's own:
  - `recurrence::build_series_control` (its done button held the Series button and its popover; its field handlers held sibling fields)
  - `room_finder::attach` (`RoomFinderInputs::handlers`)
  - the attachments group and its rows (whose buttons held the group, while the `rows` list held the rows and is now emptied at teardown)
- **App-level callback slots cleared at teardown:**
  - `RecipientEntry::clear_addresses_changed`
  - `ScheduleGrid::clear_callbacks`
  - `calendar_view::clear_day_selected` (the preview mini-calendar)
  - the Scheduling refresh-hook holder
  - the room finder's availability-refresh holder
  - `SchedulingHandle::clear_listener`: the handle outlives the editor, and its listener held the last editor's Scheduling page until the next editor opened.
- **Controllers that held their own widget:** the time grid's hover, click and context controllers (`attach_hover`, `attach_click`, `attach_time_grid_context`) now hold their canvas weakly, and the schedule grid's click gesture uses `gesture.widget()`. The main calendar's grids are long-lived, so this mattered for the editor's preview grid and the Scheduling grid.
- **Popovers attached with `set_parent`:** the room finder's popover is now unparented on its button's `destroy`, like the recipient suggestion popover.
- **An existing bug, fixed along the way:** `RecipientEntry` kept a single change callback, so with the Scheduling Assistant available (`event_editor.rs`, the Scheduling block), its callback replaced the footer's Save/Send label updater. Callbacks now add up.
- **Forward-invitation dialog (`window/imip.rs`):** its response handler clears the To field's change callback, which held the dialog and the field.

**Check:** with the `close-request` teardown disabled, `closed_event_editor_is_freed` fails again (the whole window survives).

## Composer results (2026-11-15, released as 0.9.318, not yet run in the real app)

**Measured first:** a new test, `compose::tests::closed_composer_is_freed`, opens a composer in a window with pop-out enabled, clicks Close and lets `on_done` remove it, as the compose host does. Before the fix, about 500 widgets and the WebView survived. A temporary reference-count probe then showed which widgets the closures were holding.

**Fixed:**
- **`ComposerHandlers` (`compose.rs`):** records the 25 handlers `build_compose_view` connects on the composer's own widgets, plus the autosave tick task. They are disconnected (and the task aborted) when the composer ends, so the handlers can keep their plain strong captures. The trigger is the existing `root-notify` "unrooted and not moving" path, which already stopped autosave, so it covers Close, Send, Cancel, displacement and a popped-out window closing. Root `destroy` is a fallback for a composer that was never shown. The baseline `load_changed` handler went back to a once-only flag and is now tracked.
- **Schedule-send menu:** its callback holds `send_composed` weakly. The popover belongs to the Send button, and `send_composed` holds that button.
- **`RecipientEntry`:** its own handlers capture a new `WeakRecipientEntry`: widgets weak, plain state strong. The suggestion popover, added with `set_parent(&entry)`, is now unparented on the entry's `destroy`. This affects every recipient field (composer, event editor, invitation replies).
- **`install_web_view_zoom` (`window/reading.rs`):** the key and scroll controllers use `controller.widget()` instead of capturing the WebView. This affects every composer and pop-out message window.
- **Compose host:** `on_done` also clears `draft_save_failed_tx`, whose consumer held the composer's status label until the next composer opened.

**Test-environment finding:** under Broadway, GTK's default input-method module keeps every `GtkText`/`GtkTextView` alive after it is dropped, even a bare one with no app code involved. With `gtk-im-context-simple` they are freed at once. `gtk_test::gtk_ready()` now sets that module through `gtk::Settings`, so the leak tests measure the app rather than the backend. Whether the Wayland IM module behaves the same in the real app is not known; the Phase 0 probe's `GtkText` counts would show it.

**Check:** with the two `disconnect_all()` calls disabled, the composer test fails again (the whole composer survives).

## Phase 3 results (2026-11-15, released as 0.9.317, not yet run in the real app)

**Done:**
- **`MessageListModel`:**
  - `clear_threads` now removes stale conversation stores instead of only emptying them.
  - A new `prune_sections` removes stale group stores after the root splice. Until that splice, a stale `Section` row still holds its store, so it can't be removed earlier.
  - Both maps now hold only what is on screen. Rebuilds no longer walk every conversation ever shown.
- **`sender_photo_cache` → `SenderPhotoCache`** (`window/state.rs`):
  - It holds at most 256 senders and drops the least recently used first.
  - Photos are decoded to fit 96 px (`decode_avatar_texture`, through gdk-pixbuf, falling back to a full-size decode).
  - It empties itself when `UiState::contacts_generation` changes. That counter is bumped where `contacts_by_account` is replaced or removed, so an edited photo now shows too.
  - On a miss, the contact is borrowed through the new `contacts_view::find_contact_record` rather than copied. `find_contact_by_address` now uses that helper.
- **`row_display_registry`:** dead entries are pruned each time a row registers.

**Left alone:** the `pending_optimistic_*` maps already drop their entries when a sync confirms the change, and expiring them on a timer could lose the data needed to undo a failed change.

**Tests:**
- `child_stores_are_dropped_when_their_rows_leave_the_list`
- `sender_photo_cache_is_bounded_lru_and_resets_on_contact_changes`
- `avatar_textures_are_decoded_to_avatar_size`

The full `lookout-app` run gives 835 passed and 2 failed (`display_accel_is_human_readable` and `body_cache_invalidates_a_recreated_mailbox_and_a_removed_account`, both of which already failed).

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
- `MessageListModel::clear_threads`: `retain` only live ids. (Releasing capacity on the cleared `keys` became moot: stale entries are now removed entirely.)
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
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings -A clippy::result_large_err` (plus `-A clippy::nonminimal_bool` until the side item in "Status" is fixed).
- Tests under `dbus-run-session` (see memory note), Broadway for GTK tests.
- Add GTK tests that rebuild the folder tree, calendar checklist and tasks list, then assert that a `WeakRef` to an old row widget, its menu popover and a swatch upgrades to `None` after the rebuild (and after `glib::MainContext` iterations).
- Unit test: after `clear_threads` with an empty live set, `threads` is empty.
- In the running app (only with the user's go-ahead to launch): Inspector instance counts stay flat across several unread changes and 5-minute polls, and RSS stays roughly flat over an hour, instead of +1.3 GB.
- Version bump, CHANGELOG and metainfo entry per the repo's usual release-commit pattern.
