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

## Status (2026-11-19)

| Phase | State | Release |
|---|---|---|
| 0: measure | Done: a real leak, mostly the calendar sidebar | measured on 0.9.315 |
| 1: break widget reference cycles | Done, event and task editors included; not yet run in the real app | 0.9.316 (memory steady at ~600 MB, user report); composer in 0.9.318; editors in 0.9.319 |
| 2: stop rebuilding when nothing changed | Not started | |
| 3: bound the Rust collections | Done; not yet run in the real app | 0.9.317 |
| 4: cut duplicate copies | Done for the message list, All Inboxes and birthdays (about 63% less heap per listed message). In the real app (user report, 2026-11-19): steady at about 700 MB with 7 accounts on one machine and 400 MB with 4 on another | 0.9.320 |
| 5: stop tests polluting the real cache | Done; not yet run in the real app. This machine's old leftovers were deleted on 2026-11-15: 24,865 entries (mail, message-body, attachment, calendar, EDS-mirror, webcal and Google Tasks test files); `~/.cache/lookout` went from 6.2 GB to 2.5 GB | 0.9.320 |
| Per-account fixes: background, row menus, People list | Done; not yet run in the real app (see "Per-account fixes results") | 0.9.321 |

**Next step:**
- The leak is fixed: 0.9.320 holds steady in real use (about 700 MB with 7 accounts, 400 MB with 4).
- The per-account breakdown is done (see "Per-account breakdown"). The cost follows what each account shows, not a fixed amount per account.
- Its three biggest reducible items (the full-size background, the row menus built up front, the full-size People photos and the hidden People list) are fixed in 0.9.321. Next: re-run the no-accounts and large-account probe runs on 0.9.321 to measure the saving.
- Phase 2 is the only phase not started. It is smaller and mostly saves CPU now that rebuilt rows are freed. Building the folder row menu on first open, one of its items, is done.

**Still worth checking in the running app:** the Phase 0 probe's per-type counts across a session that opens and closes composers and editors. Do `AdwWindow`, `WebKitWebView` and `GtkPopoverMenu` stay flat? Under Wayland, is `GtkText` retained by the input-method module the way it is under Broadway?

**Open side item:** this machine's clippy (1.92) flags `nonminimal_bool` at `crates/mail/src/session.rs:274`, which is older than this work. Until it is fixed, the clippy command needs `-A clippy::nonminimal_bool`.

**Line numbers:** those in "Findings" and in the Phase 1 site list are from 0.9.315, before the fixes, and no longer match the code. For example, the folder row setup at `window.rs:203-363` is now `build_folder_row`.

## Per-account breakdown (measured 2026-11-19 on 0.9.320)

**Setup:**
- **Runs:** the installed 0.9.320, run nine times for 3.5 minutes each with the Phase 0 probe: no accounts, all 7, then each account alone.
- **Settings:** each run used a temporary `XDG_CONFIG_HOME` with GLib's keyfile settings backend, seeded with a copy of the user's Lookout settings plus that run's `accounts-disabled` list. The real settings were not touched.
- **Caches:** the runs used the real caches.
- **Heap profile:** the largest account was then profiled with a sampling heap profiler (`LD_PRELOAD`, one sample per ~32 KB allocated per thread, with backtraces). It ran on a release build of the same commit with line tables.

**RSS after 3.5 minutes:**

| Run | RSS | Above baseline | malloc in use |
|---|---|---|---|
| No accounts | 301 MB | — | 47 MB |
| All 7 | 548 MB | +247 MB | 214 MB |
| Google `…1676251201_0` (28,591 messages in 61 folders, 212 contacts) | 447 MB | **+146 MB** | 136 MB |
| Google `…1749327634_1` | 332 MB | +31 MB | 66 MB |
| Google `…1676261544_5` | 329 MB | +28 MB | 66 MB |
| Microsoft 365 `…1774040807_0` | 319 MB | +17 MB | 59 MB |
| IMAP `…1788988877_0` | 318 MB | +17 MB | 56 MB |
| IMAP `…1788990354_1` | 312 MB | +11 MB | 55 MB |
| ownCloud, contacts only | 312 MB | +11 MB | 56 MB |

**What the table shows:**
- **The costs add up.** The seven single-account increases total 261 MB, against 247 MB measured with all seven on.
- **There is no fixed 100 MB per account.** The cost follows what an account shows; one account accounts for 60% of it.
- **Nothing grows.** Every count is flat across each run.
- **The baseline is mostly libraries.** About 170 MB of RSS is shared libraries and the binary: Mesa's `libLLVM` 48 MB, WebKit 24 MB, `lookout` 29 MB.

**Where the heap goes:** glibc's main arena (the GTK thread) holds about 126 MB in use for the large account. The account sessions' thread arenas hold a few MB in all. The sampled live heap is 165 MB, 144 MB of it on the GTK thread:

| Live | Allocated at | What it is |
|---|---|---|
| 37.2 MB | `window.rs:315` `Texture::from_bytes` | The bundled background `background2.jpg` (4000 × 3250 px), decoded at full size on every start whatever the window size. This is most of the no-account baseline heap. A custom background (`window.rs:326`, `:9821`) is decoded the same way. |
| 15.5 MB | `folder_menu::build_row_menu` → `ContextMenu::new` | A complete "⋯" `PopoverMenu` (about 16 items, 2 submenus, 10 swatches, 13 actions) built up front for every folder row the sidebar creates, open or not. |
| ~20 MB | `contacts_view::rebuild_contacts_list_ui` | The People list. **15.1 MB** of it is `contact_photo_texture` (`contacts_view.rs:372`) decoding each contact photo at full size (about 370 KB each) for a small avatar. The rest is the per-contact rows of a non-virtualized `ListBox`, built whether or not People is open. |
| 12.0 MB | `libglycin` (other threads) | Image decoding (GTK's glycin loaders) for the textures above. |
| 9.8 MB | `main.rs:234` (`app.run()`) | GTK internals: CSS, rendering, accessibility. |
| 5.0 MB | `calendar_view::rebuild_calendar_checklist` → `ContextMenu::new` | The calendar rows' "⋯" menus, likewise built up front. |
| < 2 MB each | others | Flagged-mail and search query results, the IMAP compression stream, the Config view, column cells. |

The message data itself does not show up near the top. The open folder is small, and Phase 4 made list summaries cheap.

**Fixes this points to, largest first:**
1. **Decode the background at display size.** Cap the longest side at about 2,560 px, or the monitor's size, via gdk-pixbuf's scaled loader, as `decode_avatar_texture` already does. This applies to the bundled and custom backgrounds alike. Expected saving: about 25-30 MB for every user, regardless of accounts.
2. **Build the folder row menu on first open** (`MenuButton::set_create_popup_func`), as Phase 2 already planned, and the same for the calendar rows. Expected saving: about 20 MB with the large account alone, and more with several.
3. **Decode People-list photos at avatar size** with `decode_avatar_texture`, and build the People list only when it's first shown. Expected saving: about 15-20 MB for an account with many contact photos, plus the glycin buffers.

## Per-account fixes results (2026-11-19, released as 0.9.321, not yet run in the real app)

**Background (37.2 MB):**
- `background_image::decode_for_display` decodes through a gdk-pixbuf loader. In `size-prepared` it scales the image down to the smallest size that still covers the target the way `ContentFit::Cover` draws it (`cover_size`). It never scales up.
- The target is the largest connected monitor in device pixels (`display_target`), or 2560 × 1600 when GDK reports none.
- The bundled image, a custom background at startup and a newly picked one all go through it. A format gdk-pixbuf can't read falls back to the old full-size decode.
- On a 1920 × 1080 screen the bundled image is now 1920 × 1560 (about 9 MB instead of 37 MB). A 4K screen saves little, since it needs about 3840 px.
- A monitor plugged in later that is larger than any at startup gets a slightly soft background until the next start.

**Row menus (15.5 MB folders, 5.0 MB calendars):**
- New `context_menu::attach_lazily` installs the menu builder as the button's create-popup function. It runs only while the button has no popover, so the menu is built once, on first open. The builder gets the button as an argument and does not capture it.
- The folder row's `RowMenu` keeps the last `refresh` until the build and then applies it. `RowMenu::connect_closed` replaces the eager popover `closed` hookup in `build_folder_row`.
- The calendar row and group menus are built the same way. A calendar row's builder keeps only the groups' ids and names.

**People (about 20 MB):**
- `contact_photo_texture` decodes through `decode_avatar_texture` to fit `CONTACT_PHOTO_SIZE` (128 px, 2× the largest 64 px avatar). This covers the People list, the details dialog and the profile card.
- The People rail and list repaint only while the People pane is mapped, in the main window or its own. New `util::DeferUntilMapped` holds the latest request while the pane is hidden and runs it on the next map. A background `None` doesn't replace an explicit bucket that is already waiting. The birthdays calendar still refreshes on every contacts snapshot.
- After People has been shown once, its rows stay until the next repaint, as before.

**Tests:**
- New: `lazy_menu_is_built_once_on_demand`, `row_menu_is_built_on_first_open_with_the_last_refresh`, `cover_size_scales_down_to_cover_and_never_up`, `background_is_decoded_to_cover_the_display_target`, `contact_photos_are_decoded_to_avatar_size` and `defer_until_mapped_runs_on_map_and_merges_while_hidden`.
- Updated so they build the menus before watching them: `checklist_rebuild_frees_the_previous_rows_and_menus` (which now also asserts that no `PopoverMenu` exists before an open), `dropped_row_menu_is_freed` and `dropped_folder_row_is_freed`.
- **Full run** of `lookout-app` under `dbus-run-session` on Broadway: 848 passed, 3 failed, 6 ignored. The three failures are the known ones; the intermittent `google_tasks` test passes on its own.

**Not yet tried in the running app:**
- the ⋯ menus opening with the right checked colour and count mode, and the folder action box staying visible while its menu is open;
- People on first open and in its detached window;
- how sharp the background looks;
- memory on the no-accounts and large-account runs, against 301 MB and 447 MB.

## Phase 4 and 5 results (2026-11-15, released as 0.9.320, not yet run in the real app)

**Measured first:** a new ignored test, `message_list::tests::resident_heap_per_message`, loads 20,000 realistic messages, then resyncs them unchanged, and reports the heap the list keeps. Each message has a 60-character subject, a 300-character preview, two text parts and, for every third one, an attachment.

| | After load | After unchanged resync |
|---|---|---|
| 0.9.319 | 113.7 MB (5,959 B per message) | 113.7 MB |
| 0.9.320 | **41.8 MB (2,191 B per message)** | 41.8 MB |

**Phase 4, done:**
- **One copy per message in the list.**
  - `EmailSummary` now derives `PartialEq, Eq`.
  - `MessageListModel.truth` holds `Arc<EmailSummary>`s, shared with:
    - the row objects (`MessageItem::Message`);
    - the row keys: `MessageRowKey` is now the `Arc`, and its equality compares the rendered fields on demand, with a pointer-equality shortcut;
    - in All Inboxes, `UiState::unified_snapshots`.
  - It's an `Arc` rather than an `Rc` because `compute_layout` runs on the blocking pool for a large mailbox.
  - `ThreadRow` is shared with its key the same way.
- **Unchanged resyncs reuse what is already shown.** `MessageListModel::intern` keeps the existing summary for every incoming message that is identical to it. The All Inboxes path interns before it stores the snapshot, so the snapshot and the list keep sharing.
- **No throwaway rows.** `splice_diff` builds row objects only for the range it splices in.
- **No copy-and-hand-back.**
  - The filter, Focused, threading and refresh toggles re-render from `truth` directly.
  - `patch_messages` replaces only the messages it edits. `patch_previews`, the follow-up flag patch and the two optimistic-restore paths use it.
  - `find_message` replaces two whole-list copies that were made to find one message.
  - Edits to `unified_snapshots` use `Arc::make_mut`, which copies only the edited messages.
- **Smaller summaries in the list.** `trim_for_list` keeps only the attachment parts of `structure`, the only parts the UI reads (the conversation view's tiles and attachment menu). The account sessions read their own copy from the cache, and no list summary is written back or sent to a session.
- **Birthdays.** `birthday_sources` copies only contacts that have a birthday, and without their photos.

**Phase 4, not changed, and why:**
- **`MailboxId` stays a `String`.** Making it `Arc<str>` would touch every crate, and with sharing it is now stored once per message.
- **`references` stays.** Threading (`core/src/thread.rs`) and search (`cache.rs`) read it, and an empty `Vec` costs no heap.
- **The flags `BTreeSet` and the 500-character preview stay.** Both are small next to what sharing saved, and the preview cap would change stored data.
- **Contacts keep two copies.** One is in `contacts_by_account`, the other in the People list's `ContactMember`s (built only when the list is rebuilt).
- **No `malloc_trim` or allocator tuning.** Phase 0 measured only 4% of the heap as free-but-retained.

**Phase 5, done:** `lookout_core::isolate_test_cache!()` (`crates/core/src/test_support.rs`) is invoked at each crate root and in the mail and Graph integration tests.
- Before a test binary's `main` runs, it points `XDG_CACHE_HOME` at `<temp>/lookout-test-cache-<pid>`. It does this from an ELF `.init_array` constructor, while the process is still single-threaded.
- It removes that directory at exit.
- Every cache path already resolves through `XDG_CACHE_HOME`, and the tests that set it themselves (`ui_state_db`, `reminders`) work as before.
- It expands to nothing outside `cfg(test)`.
- After full `lookout-core`, `lookout-mail`, `lookout-dav`, `lookout-graph` and `lookout-app` runs, nothing new appeared under `~/.cache/lookout`, and no `lookout-test-cache-*` directory was left in `/tmp`.

**Tests:**
- `rows_and_keys_share_the_truth_and_an_unchanged_resync_reuses_it`. With interning disabled, it fails at "an unchanged resync reuses the existing summaries".
- `patch_messages_copies_only_the_patched_messages`
- `list_summaries_keep_only_attachment_parts`
- `birthday_sources_keep_only_birthday_contacts_without_photos`
- `test_support::tests::test_binaries_get_a_private_cache_home`

**Full runs** under `dbus-run-session` on Broadway:
- **`lookout-app`:** 842 passed, 3 failed, 6 ignored.
  - `display_accel_is_human_readable` and `body_cache_invalidates_a_recreated_mailbox_and_a_removed_account` already failed.
  - `google_tasks::tests::offline_session_paints_cache_without_authorizing` is intermittent and passes on its own.
- **`lookout-mail`:** 286 passed, 1 failed (`ehlo_capability_scan_finds_the_dsn_keyword`, already failing).
- **`lookout-dav`:** 174 passed, 1 failed (`ical::tests::a_room_is_cutype_resource_and_round_trips`, which fails the same way on 0.9.319).
- **`lookout-core` and `lookout-graph`:** all passed.

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

**Left open at 0.9.316:**
- The composer leaked as a whole. Fixed in 0.9.318 (see "Composer results").
- The event editor (`recurrence.rs`, `room_finder.rs`) was not reviewed. Reviewed and fixed in 0.9.319 (see "Event and task editor results").
- Still open: the ribbon's More / Move to / Quick steps popovers hold themselves too, but they are singletons whose rebuilt contents free normally, so they were left alone.

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
