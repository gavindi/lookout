# Changelog

## 0.7.2 (2026-08-07)

### Added

- **Calendar**: the large Month grid's day cells are now clickable with a select-then-edit interaction: the first click selects the day (highlighting the cell with a new `.calendar-selected-cell` border) and re-anchors every view to it - the same action as the sidebar mini-calendar's day buttons, wired through a new `calendar_view::connect_main_day_selected` that forwards to both the Month and Split-view grids - and a second click on the already-highlighted day opens the New Event editor prefilled for 9am of that day (new `calendar_view::connect_main_day_activated`). Clicks landing on an event chip are left to the chip's own button, so opening an event's editor never also moves the anchor.
- **Calendar**: empty time slots in the Day/Week/Work week grids follow the same select-then-edit pattern: the first click highlights the slot (a translucent band drawn over its column), and a second click on the highlighted slot opens the New Event editor prefilled for that exact time. The slot is snapped to the nearest half hour (a pure `slot_from_point` hit-test, unit-tested, clamped so the editor's one-hour default span stays inside the day); the wiring is a new `calendar_view::connect_slot_activated` consumed by the grids' click handler when no chip is hit. The editor-invocation code shared with the toolbar's New Event button was extracted into `window::show_new_event_editor`.

### Testing

- New `calendar_view` unit tests for `slot_from_point`: gutter clicks map to no slot, column mapping across the canvas, half-hour snapping (up and down), and clamping at the day's bottom edge.

## 0.7.1 (2026-08-07)

### Added

- **Calendar**: the New Event dialog's "Invite required attendees" field now offers real autocomplete instead of plain typing. A new `window::calendar_attendee_suggestions` builds a `SuggestionSource` the same way the mail composer's To/Cc/Bcc fields already do (`merge_contact_suggestions` over mail-history hits and CardDAV suggestions) - but unioned across *every* connected account rather than scoped to one, since an invitee has no natural tie to whichever account owns the calendar being edited.

### Changed

- **Mail**: the permanent search entry in the window header bar gets a dedicated `.header-search-entry` CSS class and a dark grey background (`#202020`) via `install_paned_css`'s stylesheet, so it reads as a distinct search field against the header bar instead of blending into it.

## 0.7.0 (2026-08-07)

### Added

- **Calendar**: the "New Event"/"Edit event" dialog is rebuilt around a fuller feature set closer to Outlook's compose view. Attendees are now real: a new `RecipientEntry`-backed "Invite required attendees" field feeds a new `lookout_core::Attendee` (address + `AttendeeRole`/`AttendeeStatus`) list on `CalendarEvent`/`EventOccurrence`, round-tripped through CalDAV as repeated `ATTENDEE` properties (`ROLE`/`PARTSTAT`/`CN` parameters) via `lookout-dav`'s existing generic property mechanism, with a best-effort `ORGANIZER` emitted only when there's at least one attendee and the owning GOA account's display name looks like an email address (`window::calendar_owner_email`). A new `recurrence` module (`crates/app/src/recurrence.rs`) is a small, dependency-free RFC 5545 `RECUR` builder/parser (`FREQ`/`INTERVAL`/`BYDAY`/`COUNT`/`UNTIL`) driving a "Series" popover in the dialog's new top toolbar - previously the editor could only display that a recurring series existed, never set or change one; a rule this builder can't fully represent (`BYMONTHDAY`, `BYSETPOS`, ...) is left untouched as "Custom recurrence" rather than silently simplified. The toolbar also gains Busy/Free (`TRANSP`), Categorize (`CATEGORIES`), Reminder (a single `VALARM`/`TRIGGER`, `reminder_minutes_before` on the model), and Sensitivity (`CLASS`) controls, plus a "Video call link" toggle+URL row stored as the RFC 7986 `CONFERENCE` property (read falls back to a bare `URL` for interop) - the generic stand-in for provider-specific meeting integrations this app has no path to. Print and an "Options" overflow are disabled placeholders, matching the same `sensitive(false)` convention already used elsewhere in the calendar toolbar.
- **Calendar**: the dialog's right-hand pane is a new live preview - a mini month calendar above a single-day schedule strip - built by reusing the main Calendar tab's own `MiniCalendar`/`TimeGrid` widgets (`calendar_view::build_time_grid` widened to `pub(crate)` for this). It renders a synthetic chip for the event currently being edited alongside a read-only snapshot of whatever else `window.rs` already had cached for that month (from checked calendars only - the dialog itself never fetches anything), and updates live as the title, date, time, or calendar selection changes, auto-scrolling to the picked time via a new `calendar_view::scroll_time_grid_to_minutes`.

### Changed

- **Calendar**: the event editor's window shell moves from a bare `gtk::Window` with one two-column form grid to an `adw::Window`/`adw::ToolbarView` with a `gtk::Paned` split (form | preview), and the compact date/time pickers move from always-visible `gtk::Calendar` widgets to popovers behind date buttons. The calendar picker moved from an `adw::ComboRow` to a plain `gtk::DropDown` (inside an `adw::ActionRow`) partway through this work - `ComboRow` reserves a fixed-width value label next to its title and ellipsizes long "Account · Calendar" labels, where `DropDown`'s button and popup both size to their widest item's natural width instead.
- **Calendar**: `lookout_core::CalendarEvent`/`EventOccurrence` gain `attendees`, `organizer`, `categories`, `sensitivity`, `transparency`, `reminder_minutes_before`, and `conference_url` (all `#[serde(default)]` for backward compatibility with anything already cached on disk).

### Testing

- New `lookout-dav` unit tests cover parsing and round-tripping every new iCalendar property, including two format gotchas found along the way: `CATEGORIES` is one of `icalendar`'s hard-coded multi-value properties (lives in `multi_properties`, not the single-valued map `property_value` reads), and `icalendar` 0.17.13's own `Trigger::try_from` can't parse the negative-duration strings its own `Trigger::before_start` writes (`chrono::Duration`'s `-PT900S` output isn't accepted by the `iso8601` parser it delegates to) - reminders are parsed by hand instead, reusing the existing `parse_ical_duration` on the sign-stripped remainder. New `recurrence` module tests cover RRULE round-trips and the "can't represent this rule" bail-out. New `event_editor` tests (the dialog's first, via a new pure `build_event_from_input` extracted from the widget-reading glue) cover attendee-status preservation across an edit, organizer omission without attendees, and the video-call URL being dropped when the toggle is off even if the field still has text in it.

## 0.6.57 (2026-08-07)

### Added

- **Mail**: whole messages can now be exported as `.eml` files, backed by a new flat-file raw-message cache. The ribbon's "More" button is no longer a disabled placeholder: it opens a popover whose **Save as .eml…** action fetches the selected message's entire raw RFC 5322 bytes (`AccountCommand::FetchRawMessage` → `BODY.PEEK[]`, so `\Seen` is never set), serves repeat requests straight from `$XDG_CACHE_HOME/lookout/mail/messages/<account>/` - deterministic filenames keyed by hashed mailbox + `uidvalidity` + `uid` (the same recycled-uid guard as the attachment cache) - and opens a save dialog with the subject-derived filename suggested (a path-free `message.eml` placeholder when the subject is empty or hostile). One export is in flight at a time with the same 60-second backstop as attachment fetches; failures answer with a new `RawMessageFetchFailed` event and a toast instead of hanging. The whole-message body-fallback path (used when a message's summary has no BODYSTRUCTURE) already downloaded the full message, so those bytes are now opportunistically persisted to the same cache at no extra cost - an export of such a message is instant.
- **Mail**: the reading pane shows a List-Unsubscribe banner (RFC 2369, one-click per RFC 8058). `lookout-core` gains `parse_list_unsubscribe`, which reads the message's `List-Unsubscribe` and `List-Unsubscribe-Post` headers (case-insensitive lookup, via a new shared `header_value` helper) into at most one `mailto:` address - percent-decoded, query-stripped - and at most one `http(s)` URL, plus a one-click flag. When the headers offer an action, an `Adw.Banner` appears between the message header and the body; **Unsubscribe** POSTs `List-Unsubscribe=One-Click` to the list's URL when advertised (button disabled while in flight, "Unsubscribed" toast on a 2xx, degrading to the mailto path on failure), and otherwise opens the composer pre-filled with the mailto address and subject "unsubscribe" so the user reviews before anything is sent. Acting on the offer dismisses the banner for that message (Adw.Banner has no close button of its own); a failed POST with no mailto fallback keeps it visible.

### Changed

- **Mail**: `compose.rs`'s private case-insensitive header lookup is replaced by `lookout_core::header_value` - one shared helper for every crate that reads raw `EmailBody::headers`.

### Testing

- New unit tests: raw-message cache round-trip and `uidvalidity` guard (mirroring the attachment-cache tests), `parse_list_unsubscribe` (mixed actions, one-click flag, percent-encoded mailto, lenient angle-bracket/whitespace forms, no-action `None`), case-insensitive header lookup, and the .eml filename fallback. New fixtures (`list-unsubscribe-oneclick.eml`, `list-unsubscribe-mailto.eml`) verify the banner and both action paths through the dev-only ".eml viewer". The GreenMail integration test now APPENDs a message and asserts `FetchRawMessage` returns its raw bytes verbatim, with a second request served identically from the cache.

## 0.6.56 (2026-08-07)

### Changed

- **Build**: the bundled assets now ship in a real GResource bundle instead of bare `include_bytes!`s. A new `data/resources.gresource.xml` (prefix `/io/github/gavindi/Lookout/`) lists the nav-rail SVGs, the bundled window backgrounds, and the app icon; `build.rs` compiles it with `glib-compile-resources` into `$OUT_DIR/gres.bin`, and a new `resources` module registers it at startup (`gio::resources_register`) plus the display icon theme's resource path (`gtk_icon_theme_add_resource_path`), so `io.github.gavindi.Lookout` - the app window's icon name - resolves from the bundle even where the hicolor file isn't installed (Flatpak/snap runs, dev builds). The window's background and the nav-rail artwork load through `resources::bytes()` first, keeping the `include_bytes!` constants as fallbacks when a build machine lacks the tool, and the assets moved from `source/Assets/` to `source/data/resources/` (glib-compile-resources only bundles files inside the XML's directory).

### Testing

- A new unit test validates the embedded bundle end-to-end: the compiled `gres.bin` must carry the GVariant database magic and parse as a GResource, with the nav-rail icons resolvable through it - skipped when the build fell back to an empty marker, since the whole point of the fallback is that such builds keep working. Verified both paths against the live build: the compiled bundle registers cleanly (no warnings at startup), and a simulated missing `glib-compile-resources` (a shim binary failing on PATH) produces the marker, a build warning, and a runtime warning while the app renders the `include_bytes!` copies unchanged. One format gotcha surfaced during verification: compiled GResources are GVariant databases whose 8-byte magic is `GVariant`, not `GResource` - the original magic check would have silently taken the fallback path forever.

## 0.6.55 (2026-08-07)

### Added

- **Build**: the repository now ships its own distribution builds from CI. A new `.github/workflows/build.yaml` workflow - triggered only by pushes to the `build` branch, artifacts-only, no release publishing - runs three parallel jobs: `deb-rpm` compiles the release binary once and packages it with `cargo-deb` and `cargo-generate-rpm`; `flatpak` generates `cargo-sources.json` from `Cargo.lock` with `flatpak-cargo-generator.py` and builds the bundle with `flathub-infra/flatpak-github-actions`; `snap` builds with Canonical's `snapcore/action-build` (local LXD). All four artifacts upload as workflow artifacts.
- **Packaging**: the deb/rpm metadata lives in `crates/app/Cargo.toml` (`[package.metadata.deb]` / `[package.metadata.generate-rpm]`). Both packages install the binary plus the desktop file, AppStream metainfo, hicolor icon, and GSettings schema, and declare the runtime deps: `libgtk-4-1`, `libadwaita-1-0`, `libwebkitgtk-6.0-0` on Debian/Ubuntu, and `gtk4`, `libadwaita`, `webkitgtk6.0` on Fedora. Two packaging-tool quirks surfaced during verification and are worked around in the workflow: `cargo-generate-rpm` can't select a crate in a non-standard layout with `-p` (it resolves the package name as a path), so the rpm job runs from `crates/app` with `CARGO_TARGET_DIR` pointing back at the shared workspace target; and its asset entries must be TOML tables rather than cargo-deb's arrays, with dotted dependency names quoted (`"webkitgtk6.0"`) or TOML parses them as nested tables.
- **Packaging**: the Flatpak manifest is a step closer to a real build. The finish-args gain the modern settings D-Bus name (`--talk-name=org.gnome.Settings`, alongside the legacy `org.gnome.ControlCenter`) plus dconf access (`--talk-name=ca.desrt.dconf`, `--filesystem=xdg-run/dconf`) so preferences persist in the sandbox. The build now installs the GSettings schema into `/app/share/glib-2.0/schemas` and compiles it with `glib-compile-schemas` - previously the schema bundle `build.rs` compiles into `$OUT_DIR` was baked to a build-directory path that no longer exists at runtime inside a Flatpak, silently degrading the sandboxed app to session-only settings. The `dir` source also excludes `target`/`.git`, so builds don't copy gigabytes of local build state into the container.
- **Packaging**: a new `snapcraft.yaml` (base `core24`, strict confinement) builds the app as a snap. The snap reaches the host GOA daemon through snapd's `online-accounts-service` interface (auto-connected), so no GOA daemon is bundled inside; it also declares `dbus` plugs for `org.gnome.Settings` / `org.gnome.ControlCenter` and the usual desktop/wayland/x11/network/gsettings/password-manager-service/opengl/audio-playback set. WebKitGTK's own bubblewrap sandbox can't nest inside snap confinement, so the snap sets `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1` - the snap itself is the sandbox. The part uses snapcraft's `rust` plugin with a rustup bootstrap in `override-build` (Ubuntu 24.04's packaged rustc predates the workspace's MSRV), stages the GTK4/libadwaita/WebKitGTK 6.0 runtime libraries from the core24 archive, and installs the same desktop/metainfo/icon/schema set as the deb and rpm.

### Changed

- **Settings**: "Open Online Accounts Settings" no longer shells out to `gnome-control-center`. A Flatpak or snap can't exec host binaries, so the button (on the empty-state page, in the Config view's "Add account" row, and on its toolbar button) now runs a new `online_accounts::open_online_accounts()` helper on the worker thread: it first invokes the `launch-panel` GAction on the `org.gnome.Settings` bus name via `org.freedesktop.Application.ActivateAction` - how GNOME 48+ exposes panels, since the old `org.gnome.ControlCenter` D-Bus service no longer exists there - then falls back to the legacy `org.gnome.ControlCenter.ActivatePanel` call for GNOME 47 and older, and finally to spawning `gnome-control-center online-accounts` when running unsandboxed. The helper uses `zbus` (new direct dependency of `lookout-app`, already in the lockfile via `lookout-goa`, so `cargo-sources.json` is unaffected).

## 0.6.54 (2026-08-07)

### Changed

- **Mail**: full-text search moves out of the list into the window header bar. The search field is now a permanent `gtk::SearchEntry` packed at the header bar's start (its 62px start margin lines its left edge up with the menu bar's first item below, since the nav rail shifts the menu bar that far right of the header's own full-width left edge), instead of a `gtk::SearchBar` that had to be revealed by a toolbar button between the list header and the column headers - so the query and its field are always on screen, and the Search button (and the SearchBar's key-capture setup, which existed only to route Esc) are gone. Ctrl+F now focuses the entry directly from anywhere in the window and selects its existing text, so typing replaces the old query instead of appending to it. The list-level wiring is unchanged in spirit - typing debounces 300ms into instant FTS results with the live IMAP pass catching up, Esc or clearing the field exits search and restores the previous view - but a search started from *any* module now also activates the Mail nav button, re-running the module handler so the results land on screen, where the old bar could only be revealed while Mail was already showing. Esc in the entry works through `GtkSearchEntry`'s own `stop-search` signal, which survives without the bar that used to capture it.

## 0.6.53 (2026-08-07)

### Added

- **Mail**: inline `cid:` images now render in the reading pane. HTML newsletters embed their images as MIME parts referenced by Content-ID (`<img src="cid:logo123">`), which WebKit can't load on its own; the reading pane's WebView now registers a custom `cid` URI scheme, so those references come back to the app instead of failing the load. The scheme handler fires on a WebKit worker thread, so it only forwards the request to the main loop, which resolves the reference against the `cid:`-bearing parts of the message currently on screen and asks the account session for that part's bytes on demand (`FetchAttachment` - served from the flat-file attachment cache on repeat visits). Resolution tolerates the ways senders mangle the reference: exact Content-ID match, the angle-bracketed `<id@host>` msg-id form, percent-encoded characters (`logo%40123`), and a reference that drops the host of a full msg-id (`cid:logo123` resolving `<logo123@host.example>`). Every request is answered - with the transfer-decoded bytes finished into WebKit as the part's MIME type, or with an error for an unknown id, a failed fetch, the 60-second timeout backstop, or the user navigating away mid-load - so a missing image renders as a broken image instead of hanging the pane, and inline images keep loading even when "Load images from the web" is off (they're part of the message, not remote content). The matching ladder lives in `lookout_core::cid_matches`, shared by every path that resolves references.

### Changed

- **Mail**: inline images no longer appear in the attachment strip. Both body paths (BODYSTRUCTURE-driven and whole-message fallback) previously classified a named non-text part as an attachment regardless of its disposition, and senders routinely attach `filename=` to an inline image - so a newsletter's embedded graphics showed up as Save-able rows. A part is now an attachment only when it declares `Content-Disposition: attachment`, or carries a filename without an explicit `inline` disposition, so inline `cid:` images stay in the HTML body where they belong.

### Testing

- New fixtures (`html-cid-hosted.eml`, `html-cid-encoded.eml`) join `html-cid-image.eml` to cover hosted Content-IDs and percent-encoded references, and the cid matcher gains unit tests for every ladder rung (exact, msg-id brackets, percent-decoding, local-part fallback, and rejections). The whole-message fallback path's inline-image listing is now asserted with real section numbers and the non-attachment classification. The GreenMail integration test APPENDs a multipart/related message with a base64 PNG and verifies the session surfaces the cid'd part as non-attachment and returns its transfer-decoded bytes on demand. The dev-only ".eml viewer" rewrites `cid:` references to `data:` URIs straight from the raw message, so inline images verify in-app without a server.

## 0.6.52 (2026-08-07)

### Changed

- **Mail**: the reading pane's header is restyled closer to Outlook's. The subject now sits in its own full-bleed, tinted strip at the top of the pane (`message_header::MessageHeader::subject_bar`), divided from the sender block below by a border rather than sharing its margins. Sender name and address collapse onto one line (`Name<address@example.com>`, both ellipsizing so they can't force the pane wider), and the date moves off the sender row down to the `To:` row's trailing edge. Reply/Reply All/Forward switch from bare icon buttons to flat, labelled ones (`adw::ButtonContent`), and the cluster grows three disabled placeholders - a message-theme toggle, a contact-card button, and a `…` overflow, divided from the reply actions by a separator - wired the same way the top command toolbar's own disabled `more_button` is, ahead of the functionality landing separately. A second Reply/Forward pair now repeats at the foot of the message body (`action_bar`), inside the same crossfading `"message"` stack page as the header, so it appears and fades with everything else and stays absent on the empty/multi-select/composer pages. Locating the body content stack for rendering used to rely on it being the reading pane's structural *last* child (`page.last_child()`) - a second bottom bar living below it would have made that lookup return the wrong widget and silently blank the pane - so `render_body` and the attachment-strip lookup now both go through a new `find_named_child` walk keyed on `widget_name()` (`"body"`, `"attachments"`) instead.

## 0.6.51 (2026-08-07)

### Changed

- **Mail**: the folder pane and message list pane remember their widths, as a percentage of the application window's width, in GSettings (`pane-folder-width-percent`, `pane-message-list-width-percent`). A drag's new width is stored once it settles (150 ms after the last `position` change), and when the window itself is resized - interactively, maximized, tiled, or restored, detected through the window surface's `width` rather than `GtkWindow.default-width`, which GTK only updates for plain resizes of resizable windows - each stored percentage is reapplied against the new window width. The reapplied width is clamped to the same bounds the drag abides by: the start child's minimum width and the space the end child needs, so a stored proportion from a wide window can never push a pane past the reading pane's own floor on a narrow one. A pane that isn't on screen (the Mail tab hidden behind another module) is skipped, so a clamp can't be applied to it and silently overwrite the stored percentage with the clamped value. The stored values survive restarts through the existing GSettings schema (the in-memory fallback gets the same keys and defaults, and a never-set percentage, `-1`, leaves the pane at its built-in position).
- **Mail**: the folder pane is now bounded on both sides: its minimum width goes from 150px to 200px, and it gains a 320px maximum. The floor is the pane's `size_request` (which is also what the drag and the resize-reapply clamp to), and the ceiling is enforced in the paned's `position` handler - GTK only limits the drag to the pane's natural bounds, which grow with the window, so any overshoot past 320px is snapped back and the separator stops there no matter how wide the window gets. On a window too narrow to give the pane 320px, the natural end-child floor wins.

## 0.6.50 (2026-08-07)

### Added

- **Mail**: the attachment strip's Save button is now a menu with three actions - **Open**, **Open With…**, and **Save…** - all three sharing the same on-demand part fetch. **Open** resolves the MIME type's *default* application through GIO (`AppInfo::default_for_type`) and launches it directly (`AppInfo::launch`) - a plain activation with no chooser and no portal - so a file whose type has a registered default opens in exactly that app; only when no default exists for the type (or the direct launch fails, e.g. running sandboxed) does it fall back to `GtkFileLauncher` and the XDG portal. **Open With…** asks which application to use through the freedesktop portal itself - a direct `org.freedesktop.portal.OpenURI.OpenFile` call (the portal's `OpenURI` method explicitly rejects `file://` URIs, so the temp file is passed as a file descriptor) with the `ask` option set - presenting the portal's own application chooser, which is what makes the choice work from inside a sandbox. Both open from a unique temporary file (`$TMPDIR/lookout-<uuid>.<ext>`, the extension chosen from the filename or a content-type map so the handler recognizes the type); temp files are registered in `UiState::temp_attachment_files` as soon as they're written and deleted when Lookout exits (`app.connect_shutdown`) - the viewer never outlives its file - and any write or launch failure cleans up immediately with an error toast. **Save…** is the existing save-location dialog. One action is in flight at a time, tracked by the same `PendingAttachment`/`PartFetched` machinery as Save, with the same 60-second backstop so the row's button can never be stuck.

### Changed

- **UI**: the left nav rail's Mail/Calendar/People buttons now use bundled full-colour SVG artwork (`source/Assets/icons/email-1.svg`, `calendar-1.svg`, `contact-1.svg`), decoded from the binary through a new `nav_rail_image` helper, instead of the machine's icon theme (`x-office-calendar-symbolic`/`avatar-default-symbolic`, and the bundled app icon for Mail) - so the rail renders identically on every machine regardless of the installed icon theme.

## 0.6.49 (2026-08-07)

### Added

- **Mail**: attachments can now be saved to disk. The reading pane renders an attachment strip between the message header and the body - one row per attachment (`BodyPart` metadata only, never bytes): an icon, the filename (with a part-number fallback for unnamed parts), a human-readable size, and a Save button. Saving is fully on demand: a new `AccountCommand::FetchAttachment` asks the session for that one part's bytes (`UID FETCH BODY.PEEK[<part_number>]`), the wire bytes are transfer-decoded (`base64` with CRLF folding, `quoted-printable` hex escapes and soft line breaks - new `transfer_part_bytes` in `lookout-mail`), and the decoded content lands in a per-account flat-file cache under `$XDG_CACHE_HOME/lookout/mail/attachments/`, keyed by hashed mailbox + `uidvalidity` + `uid` + part number (deterministic, collision-free, and guarded against recycled uids like the body cache), so a re-save is instant and survives restarts. The session answers with `AccountEvent::PartFetched`; the UI re-enables the row and opens a `Gtk.FileDialog` save dialog (which routes through the XDG portal in sandboxed runs) with the attachment's own filename suggested, toasting "Attachment saved" on success and an error toast on a write failure. One fetch is in flight at a time; a Save click while one is outstanding is ignored.

### Fixed

- **Mail**: saving an attachment could leave the Save button stuck on "Fetching…" forever. The on-demand fetch targets a part by its IMAP section path (`BODY.PEEK[<n>]`), and the whole-message fallback body path (`parse_body`, used whenever a message's summary carries no BODYSTRUCTURE-derived structure) numbered attachment parts by enumerate counter - "0", "1", "2" - rather than by their real section paths. That matters most for messages with no text parts at all (a pure-attachment mail, e.g. a fax or a document notification): the partial-fetch path has nothing to fetch and always falls back to the whole message, so its attachments were always numbered "0", "1", … and `BODY.PEEK[0]` is not a valid section - the server errored or returned nothing, and with no answer the button never recovered. `parse_body` now computes every part's real RFC 3501 section path by walking the parsed MIME tree (root single part is "1"; multipart children number 1..N in order; an embedded `message/rfc822` is a leaf), so the fallback path's part numbers match the server's exactly. The body cache's on-disk format version is bumped (2 → 3) to wipe once any bodies cached with the old bogus numbers, so a re-open re-assembles them with correct paths instead of serving the broken ones forever.
- **Mail**: an attachment fetch that failed no longer takes the whole account session down with it. A fetch error propagated via `?` out of the command handler and killed the session - which reconnected with backoff and lost the command, leaving the Save button stuck with no explanation - and a missing section was silently dropped the same way. The session now answers every `FetchAttachment` with either `PartFetched` or a new `PartFetchFailed { mailbox, uid, part_number, message }` event, so the UI restores the Save button and toasts the specific failure without costing the connection. As a last-resort backstop, the Save button also gives up after 60 seconds if no answer at all arrives (the only remaining case: the connection dying mid-fetch and the command being lost to the reconnect), restoring itself with a "timed out" toast.

### Changed

- **Build**: `lookout-core`'s vCard writer is renamed `VCard::serialize` (was `to_string`) and the module's `get(0)`/redundant-closure spots are cleaned up - pure clippy hygiene under the current stable toolchain, whose newer lints had pushed the workspace past CI's `-D warnings` gate.

### Testing

- New fixtures cover the fallback path's part numbering: `with-attachment.eml` (multipart/mixed - the pdf is section "2"), `nested-parts.eml` (a multipart/alternative inside multipart/mixed - the attachment is "2", the alternative's halves are "1.1"/"1.2" and not attachments), and `attachment-only.eml` (a single-part binary message - its attachment is section "1"). `transfer_part_bytes` gains base64-with-CRLF, quoted-printable, 7bit/binary-passthrough, and decode-failure fallback tests; the attachment cache gains store/load round-trip and uidvalidity-guard tests. The GreenMail integration test now APPENDs a base64-attachment message over a raw plain-TCP IMAP session and asserts the session returns the transfer-decoded bytes.

## 0.6.48 (2026-08-06)

### Added

- **Settings**: the app's scalar preferences now survive restarts. A new GSettings schema (`data/gschema/io.github.gavindi.Lookout.gschema.xml`, id matching the application id) holds them, and `settings.rs` resolves it once at startup: found in the system install first (the path a packaged install takes, `/usr/share/glib-2.0/schemas`), then in a bundle a new `build.rs` compiles into `OUT_DIR` with `glib-compile-schemas` and registers at runtime as an extra schema source (the path a `cargo run` dev build takes - a compile failure only logs a warning). A machine with neither degrades to a session-only in-memory store seeded with the schema's defaults, which is exactly the pre-GSettings behaviour, never an error. The store exposes typed get/set over `gio::Settings`; every preference is written through on change and re-applied at startup, so the session opens where the last one left off: the View tab's Layout toggles (Folder pane / Reading pane / Calendar overview), Config → Appearance's "Animate transitions", the message list's sort key and direction, the folder tree's Favorites section, and the Config → Mail switches ("Load images from the web", "Rich text") - all previously session-only.
- **Settings**: the relational config half has a home and a tested shape. `app_config.rs` defines the serde `AppConfig` (`$XDG_CONFIG_HOME/lookout/settings.json`): `identities` and `folder_role_overrides`, empty until the multi-identity and folder-role-override roadmap features land, with best-effort load/save like the other config files. Tags and calendar colours keep their existing JSON files.
- **People**: starred contacts now survive restarts. The Favourites state that was session-only (`UiState::starred_contacts`) persists in a new per-machine SQLite database (`$XDG_CACHE_HOME/lookout/ui-state.sqlite`, `ui_state_db.rs`), loaded at startup and written through on every star toggle. It's deliberately a separate database from the per-account mail caches: a contacts-only GOA account never gets a mail `Cache` handle, and Config → Advanced's "Clear all caches" wipes the mail/calendar caches - favourites are a preference, not a cache, so they're untouched. It follows the mail cache's `PRAGMA user_version` format-version convention: a schema bump wipes the table once.

### Changed

- **Settings**: the two preferences that already persisted on best-effort plain files moved into GSettings: the folder pane's last-open view (`last-view-unified`/`last-view-mailbox`, replacing `last-view.json`) and the window-background image path (`window-background-path`, replacing `background-image-path`). Both keep their existing public APIs, so the window code is unchanged. Machines upgrading from a plain-file build get a one-time import - only while the new keys still hold their defaults, so a choice already made under the new build is never clobbered - and the legacy files are then deleted.
- **Testing**: new unit tests cover the settings store (bool/string/strv round trips and the schema defaults on the session-only path), the UI-state database (round trip, idempotent star/unstar, and the version-bump wipe), both legacy-file migrations (import-once, stale-file-dropped), and the `settings.json` round trip plus broken-file tolerance.

## 0.6.47 (2026-08-06)

### Changed

- **Mail**: opening a message no longer downloads the whole RFC 5322 message including attachments. The envelope sync now requests `BODYSTRUCTURE` and flattens it into a per-message part list (`EmailSummary::structure`, persisted in the envelope cache - `has_attachment` is now real, derived from it instead of hardcoded false), and the viewer's body fetch is `BODYSTRUCTURE`-driven: one `UID FETCH` pulls `BODY.PEEK[HEADER]` plus every `text/plain`/`text/html` part by its part number, and attachment parts (images, documents) are never downloaded - their metadata (filename, size, part number) is still available via `EmailBody::parts` for a later on-demand fetch. Each fetched part is decoded by wrapping it in a minimal single-part message carrying its own Content-Type/charset/transfer-encoding and re-parsing with `mail_parser`, so charset and base64/quoted-printable handling is the same battle-tested path the whole-message parse used; `multipart/alternative` picks the real HTML over the plain part's synthesized rendering. The background body prefetch learns each message's structure in its envelope pass, so it warms text-only bodies too - a marketing mail's megabytes of images are never pulled into the cache. Messages whose summaries predate this (or servers that omit `BODYSTRUCTURE`) fall back to the old whole-message `BODY.PEEK[]` fetch, as do partial fetches that fail or decode nothing readable. The on-disk body cache changed format accordingly (raw bytes → assembled `EmailBody` JSON; a version-2 wipe drops old raw rows once) and the FTS search index now upgrades from the assembled text rather than a raw re-parse.

## 0.6.46 (2026-08-06)

### Added

- **Calendar**: the calendar is no longer read-only - the "New Event" toolbar button (previously a disabled placeholder) now opens a modal event editor, and clicking an event in *any* view opens it for editing or deletion. The editor (`event_editor.rs`) has a title field, a calendar picker listing every connected account's calendars ("account · calendar", defaulting to the first *checked* one, locked while editing), an all-day toggle, start/end date pickers with `hh:mm` spin buttons (the times grey out while all-day, whose end stays exclusive per RFC 5545), a location field, a notes area, and Save/Delete buttons - a destructive Delete appears only when editing. "New Event" prefills for the displayed date (the next whole hour, or 9:00 on a non-today anchor); edits carry over the event's full identity. Click entry points were added to all five views: the month grid's chips (now buttons, chrome-stripped by a `.calendar-event-chip-button` CSS rule so they still render as colored chips), the Day/Week/Work week canvases (a `GestureClick` reusing the existing `hovered_chip` hit-test), and the agenda rows.
- **Calendar**: edits and creates are written back over CalDAV. A new event is `PUT` to a client-generated `<uid>.ics` href under the chosen collection with `If-None-Match: *`; an edit `PUT`s to the resource's own href with its `getetag` as `If-Match`, so a write based on a stale copy fails with HTTP 412 instead of clobbering a concurrent change; delete is a conditional `DELETE` with the same etag guard. `EventOccurrence` now carries the resource href/etag (plus description/location/RRULE) so a clicked occurrence is the complete edit target without a separate lookup. The session resyncs the on-screen month after a write, so the change renders through the existing `OccurrencesUpdated` path; failures surface as error toasts, and a write issued while the connection is backed off gets an explicit "not connected" error rather than being dropped silently.
- **Calendar**: recurring events can be edited as a whole series. The master's RRULE is preserved verbatim on save (`build_vcalendar` serializes it back unchanged), and the editor prefills the *series anchor* - the master `DTSTART`/`DTEND`, carried on every occurrence as `master_start`/`master_end` - rather than the clicked occurrence's expansion, so renaming a recurring event can't silently re-anchor the whole series. Per-occurrence edit scopes remain a later TODO. New events always get a fresh UUID `UID`.
- **Testing**: the iCalendar serializer and the write verbs are now covered: `build_vcalendar` round-trips through the parser (timed, all-day, recurring, and the href/etag metadata path), and `lookout-dav` gains wiremock tests for PUT with `If-None-Match`/`If-Match` (including etag-quote normalization and the 412 body-snippet error) and conditional DELETE.

## 0.6.45 (2026-08-06)

### Added

- **Mail**: full-text search across all synced mail, Gmail/Outlook-style. A `gtk::SearchBar` between the message-list header row and its column headers is revealed by a new Search button in the Home toolbar or by Ctrl+F (a `ShortcutController` on the window, so it works from any focus); typing - debounced 300 ms so a burst of keys is one search, not one per keypress - flips the list into a search mode and shows results instantly from a per-account SQLite FTS5 index (`search_fts`, using the bundled SQLite's FTS5), with the open mailbox's live IMAP `SEARCH` pass catching up a beat later for mail the local index hasn't seen. The index covers subject, sender, recipients, and body text: indexed from the envelope on every sync, upgraded to the full message text once a body is fetched (bodies over 256 KB stay preview-only rather than paying a re-parse), and rewritten only for rows whose text could change, so a flag/keyword `STORE` doesn't touch it. Queries AND their bare words and honour `"`-quoted phrases; FTS operator characters are neutralised, so typing `AND` searches for the word "and" rather than altering the query. Snoozed messages are excluded, matching the list. Results span every account and every synced folder; clicking a result opens it from any folder, because `FetchBody` now SELECTs its mailbox on demand instead of dropping non-current folders. Esc, clearing the field, or clicking a folder leaves search and restores the previous view (the open mailbox, or All Inboxes if search started there). The live pass is `AccountCommand::SearchMailbox` (`UID SEARCH TEXT` + envelope fetch, SELECTing on demand) covering the currently-open mailbox - or every account's Inbox from the unified view - and it always answers with an `AccountEvent::SearchResults` even for zero hits, so the UI can tell "searched, nothing found" from "still searching".
- **Mail**: caches written before the search index existed are backfilled once on open, so old cached mail is searchable without forcing a full re-sync. The backfill deliberately runs on the account session's worker thread, not in `Cache::open` itself - the app also opens a read-side cache handle from the UI thread at connect time (for composer autocomplete), and re-parsing every cached body to index it would have frozen startup on a large pre-existing cache. It's a cheap no-op (a single count query) once the index exists, and idempotent under a re-run.

## 0.6.44 (2026-08-06)

### Added

- **UI**: the message list now groups mail older than a year under headers named for the message's own calendar year ("2025", "2024", …), newest year first, instead of lumping it all under the catch-all "Older" section. Anything older than 12 months gets a year header; mail from roughly a month to a year ago still reads "Older". The cutoff is by *age*, not calendar year - a December 2025 mail is still "Older" in August 2026 even though a July 2025 mail is already "2025". `DateBucket` gains a `Year(i32)` variant, its one payload-carrying member; that's safe where a dated month variant was deliberately avoided: an old message's year is fixed for good, so a user-collapsed `Year(2024)` section can never silently stop matching as the calendar rolls over. Sections cut as consecutive runs of the date-sorted list, so the year headers fall out newest-first under a descending sort and oldest-first under an ascending one, exactly like the recent sections.

### Changed

- **Mail**: the message list is no longer limited to a folder's most recent messages. `sync_mailbox` now fetches the *whole* folder (`FETCH 1:*`) on every sync, so a folder with more than a couple of hundred messages - Gmail's All Mail in particular - shows all of it rather than a recent window. `INITIAL_FETCH_LIMIT` (200) is re-scoped to the background body prefetch only, which still queues bodies for just the newest messages while anything older fetches on demand. Full CONDSTORE/QRESYNC incremental sync remains Phase 2, so every sync is still a full re-fetch; the price of completeness is that a resync of a very large folder (a new-mail IDLE wake while it's open, Refresh, or a post-move/snooze resync) re-pulls its whole envelope set. One finding from verifying against live accounts: Gmail's `STATUS (MESSAGES)` over-reports All Mail (~14,850) versus what a `SELECT`/fetch actually returns (~12,296) - a Gmail-side count-vs-retrievability gap no IMAP client can bridge, so the folder-tree count can read higher than the list's rows.

### Fixed

- **Mail**: a Gmail inbox (or any folder) could silently hide messages that were still in it. Every sync fetched an envelope *UID* window - `UIDNEXT - 200 : *` - on the assumption that the newest 200 UIDs cover a mailbox's contents. On Gmail that assumption is false: the UID counter is effectively account-global (All Mail traffic drives it up) while messages linger in the Inbox with UIDs far older than `UIDNEXT - 200`, so those survivors were never fetched, never cached, never shown - an inbox reporting 22 messages on the server displayed only its 6 newest. Both `sync_mailbox` and the background body prefetch now window by message *count* (the newest N sequence numbers) rather than UID, which always covers exactly the messages currently in the mailbox, so low-UID survivors are fetched like any other message.
- **Mail**: a folder whose envelope cache predated the full-sync change could keep hiding its older mail forever. The cache-hit path, which serves a folder's cached list on switch instead of re-syncing, treats any non-empty cache as complete - and pre-fix caches held only a windowed subset, so once a folder had been synced under the old code, opening it would keep serving that subset with no live sync to correct it. `Cache::open` now bumps a stored on-disk format version (`PRAGMA user_version`) and wipes the `messages` table once when the version changes, forcing every folder to re-sync in full on its next open. Message bodies, snooze entries, the address book, and the mailbox list are all untouched.

## 0.6.43 (2026-08-06)

### Added

- **Mail**: the message list can now select more than one message at once. The list's backing model switched from `Gtk.SingleSelection` to `Gtk.MultiSelection`, so `GtkListView` handles ctrl-click (toggle) and shift-click (range) natively; a new **Select** toggle button, placed right after the favorite star in the message-list header, is a second input onto that same selection. Toggling Select swaps every row's two-letter avatar for a checkbox in the exact same 32x32 slot - `select_mode_button.bind_property("active", ...)` bindings on each row's avatar/checkbox visibility (rather than a CSS class or a rebind) keep the swap live across every row, including ones already on screen, with no extra column and no reflow. `MessageListModel::selected_summaries()`/`SelectionKind::Multiple` generalize the previous single-message API; `selected_summary()` keeps its exact old meaning (`Some` only for exactly one message), so Reply/Reply-All/Forward/Categorize correctly no-op on a multi-selection with no changes of their own.
- **Mail**: Delete, Archive, Report, Flag/Unflag, Snooze, and a **new** Mark read/unread toolbar button (there was previously only the implicit mark-on-open) all now act on the whole selection. A new `selected_message_command_targets` resolver groups the selection by `(account, mailbox)` and sends one new plural `AccountCommand` per group (`MoveMessages`/`SnoozeMessages`/`StoreFlagsMany`) - one batched IMAP `MOVE`/`COPY`/`STORE` over a comma-joined UID sequence-set and one resync per mailbox touched, not one round trip per message. Flag/Unflag and Mark read/unread compute a Gmail-style aggregate direction once over the whole selection (any unflagged/unread message selected acts on all; only all-flagged/all-read flips the other way), which reduces to exactly today's per-message behavior when just one message is selected. The reading pane shows a new "N messages selected" placeholder whenever more than one message is selected, and structurally skips mark-as-read/body-fetch in that case - `selection_kind()` only ever produces `SelectionKind::Message` for exactly one selected row.

### Fixed

- **Mail**: `MoveMessage`/`SnoozeMessage` required the target mailbox to already be the account session's currently-`SELECT`ed one, and silently dropped the command otherwise - unlike `StoreFlags`, which already selects on demand. Acting on a message from a mailbox other than the one currently open (most visibly from the unified "All Inboxes" view) could therefore silently do nothing. Both now select on demand too, matching `StoreFlags`' existing contract.

### Changed

- **Mail**: `Gtk.MultiSelection` doesn't autoselect row 0 the way `Gtk.SingleSelection` did. This is a deliberate, desired side effect of the model switch: a freshly-opened or filtered-to-empty folder can now legitimately show no highlight, matching Gmail/Outlook rather than always forcing a highlight onto some row.

## 0.6.42 (2026-08-06)

### Added

- **Mail**: messages can now be color-tagged (`$Lookout-tag-*` IMAP keywords), Outlook-style. A tag is a client-side `{key, name, color}` definition persisted as `$XDG_CONFIG_HOME/lookout/tags.json` (the same best-effort convention as calendar colours); assigning one to a message stores the keyword `$Lookout-tag-<key>` on it server-side via a new `AccountCommand::StoreKeywords` (`STORE +FLAGS.SILENT` / `-FLAGS.SILENT` for raw keyword atoms, sharing the session's `store_raw_flags` helper with the system-flag path and patching the cache through `Cache::update_keywords` so the row repaints immediately without a resync). Keywords were already parsed into `EmailSummary.keywords` by the envelope fetch and persisted in the cache, so assigned tags survive restarts and re-syncs for free. Two assignment surfaces share one menu builder: a **Categorize** button in the command toolbar (its popover is rebuilt on every open, so the checkmarks track the selected message) and a **right-click context menu** on message rows. Tagged rows draw up to three small color dots beside the subject, colored by per-tag CSS rules kept in sync with the definitions; the list's rebuild-skip check includes the tag-keyword subset so a toggle repaints the row, and `MessageListModel::refresh` re-renders after a recolor or rename (which change no message's keywords). A "Manage tags…" dialog - reachable from either surface - adds, renames, recolors, and deletes tags; the tag `key` is derived once from the name at creation and stays stable across renames so keywords already on the server keep matching. Deleting a tag is non-destructive: only the definition is removed, and the stored keywords simply stop displaying. Keys are sanitized into legal RFC 3501 atoms (`lookout_core::sanitize_tag_key`), and the session additionally drops any malformed atom before it reaches the wire; servers must advertise `\*` in `PERMANENTFLAGS` to store arbitrary keywords, and a rejection surfaces through the existing error toast.

### Fixed

- **Mail**: deleting (or archiving/reporting) a message took ~5 seconds to disappear from the message list. The MOVE itself is a single fast round trip, but the list only ever repainted on a `MessagesUpdated` event, and the move path emitted that only *after* re-listing every folder and re-fetching the whole envelope window (`relist_folders` + `sync_mailbox`) - seconds of network round trips. The `MessageMoved` event sent right after the MOVE merely showed a toast. The session now drops the moved message from the SQLite cache (`Cache::delete_message`, which also removes its cached body and any snooze entry) and republishes the remaining cached set as a `MessagesUpdated` immediately, so the row vanishes the instant the MOVE succeeds; the authoritative resync still runs afterwards to correct counts, and its emit is byte-identical to the optimistic one, so the list rebuilds once and never flickers.

## 0.6.41 (2026-08-06)

### Added

- **People UI**: the People screen's left pane is now a real account/category tree instead of a flat "account - address book" list. Each connected account gets a header followed by four fixed rows - Your Contacts (every card for the account), Favourites, Your contact lists, and Deleted - and a trailing "Categories" section lists every distinct vCard `CATEGORIES` tag found across all accounts, each filtering the right-hand list independently. Header rows are non-selectable; each selectable row stashes its index into the underlying bucket list on itself (`set_data`), since mixing headers into the list means a row's widget position no longer matches its data index.
- **People/Favourites**: contacts can be starred from a toggle button on each row in the right-hand list. Purely local/session state (`UiState::starred_contacts`, keyed by account + vCard `UID`) - never written back to the vCard or synced to the server - and evaluated live when the Favourites bucket renders, so toggling a star doesn't require rebuilding the whole left pane.
- **People/Deleted**: contacts that vanish between one CardDAV poll and the next are now accumulated into a per-account `Deleted` bucket (`UiState::deleted_contacts`), diffed by vCard identity against the previous sync's snapshot. In-memory only, cleared on restart - there's no real CardDAV deletion-tracking protocol behind it, just "it was here last poll and isn't now."
- **DAV tracing**: every outgoing PROPFIND/REPORT request body and its raw response body/status are now logged at debug level, and CardDAV contact sync logs a per-address-book and per-account vCard count. Diagnosing three consecutive CardDAV interop bugs against live Google/Nextcloud servers this cycle was only possible once the actual wire bytes were visible instead of a bare HTTP status.

### Changed

- **People UI**: the tab is now labelled "People" throughout (nav-rail tooltip, toolbar label, page heading, empty-state copy) rather than "Contacts". Internal type/module names are unchanged.
- **CardDAV/contacts fetch**: fetching an address book's vCards no longer uses a `sync-collection` REPORT (RFC 6578) or an `addressbook-query` REPORT (RFC 6352 §8.6) with an unconditional empty filter. It now does a `PROPFIND` (Depth: 1) to enumerate the collection's member hrefs, then an `addressbook-multiget` REPORT (RFC 6352 §8.7) to fetch their vCard bodies. Confirmed against a live Google account: Google's CardDAV returns a bare, response-less `<multistatus/>` for an empty `<filter/>` - i.e. it reads "no filter" as "match nothing", the opposite of RFC 6352's own example - so multiget, which names specific known resources instead of relying on any filter semantics, is the reliable choice. This app never stored a sync token between polls anyway (every 15-minute poll was already a full refetch), so `sync-collection` bought no incremental-sync benefit it was actually using.
- **`ContactsAccountSnapshot`**: dropped the intermediate `ContactsCategorySnapshot` (one bucket per discovered CardDAV address book) in favor of a single flattened `contacts: Vec<VCard>` per account - the left pane no longer organizes by address book, so per-book identity had nothing left to consume it.

### Fixed

- **CardDAV discovery**: `discover_addressbook_home`'s PROPFIND for `addressbook-home-set` asked for the prop `CD:addressbook-home-set` without ever declaring the `CD` (CardDAV) namespace prefix on the request body - malformed XML that at least one real CardDAV server (Nextcloud) rejected outright with an HTTP 400 before it ever reached CardDAV handling. `build_propfind_body` now declares `xmlns:CD="urn:ietf:params:xml:ns:carddav"` alongside its existing `D`/`C`/`IC` namespaces.
- **CardDAV sync**: `build_sync_collection_body`'s initial (no sync-token) request omitted the `<D:sync-token>` element entirely instead of sending it empty. RFC 6578 requires the element to be present - empty, meaning "start from scratch" - even on a cold-start sync; Google's CardDAV rejected the omitted form outright with a generic `INVALID_ARGUMENT`. Fixed regardless of the subsequent switch away from `sync-collection`, since the function (and the RFC compliance bug) remain available for future incremental-sync use.
- **vCard parsing**: `VCard::parse` rejected any `VERSION` other than `4.0`, hard-erroring on the extremely common `VERSION:3.0` (RFC 2426) - which is what Google's CardDAV, among others, actually exports. Worse, that single parse error propagated via `?` out of `fetch_addressbook_vcards` and discarded every other successfully-parsed vCard in the same response, so one legacy-version card zeroed out an entire account's contacts with no error surfaced beyond a single warning log. `VCard::parse` now accepts both `3.0` and `4.0`, and `fetch_addressbook_vcards` skips and logs an individual unparseable card instead of aborting the whole batch.
- **GOA/Contacts discovery**: added debug logging of each GOA account's `Contacts` interface presence, `Uri`, and auth interface, used to diagnose why a Microsoft/Outlook.com account with Contacts toggled on in GNOME Online Accounts never appeared in the People tab - GOA does expose a `Contacts` interface for it, but with an empty `Uri`, which `parse_contacts_account` (correctly) treats as "no usable contacts endpoint" and silently excludes. Microsoft/Outlook.com has no native CardDAV endpoint for `goa-daemon` to populate, so this is a real capability gap rather than a bug; supporting it would need a separate Microsoft Graph API contacts integration, the same shape of special-casing this app already does for Microsoft 365 Mail.

## 0.6.40 (2026-08-06)

### Added

- **Contacts UI**: a new Contacts module is now available from the left navigation rail, with its own page in the main content stack and module-specific toolbar routing.
- **Contacts UI**: the Contacts page now uses a split-pane layout: a left-side category list and a right-side contact list. The left pane is populated per account with an "All contacts" entry plus each discovered CardDAV address book category.
- **Contacts UI**: selecting a left-side category now filters the right-side list immediately, and activating a contact row opens a modal details dialog showing the contact's full vCard-derived information (emails, phones, addresses, organization/title, and notes).

### Changed

- **Contacts/CardDAV**: synced CardDAV contact data is now retained as richer per-account snapshots (account label, category buckets, and flattened suggestion addresses) so the same source feeds both the Contacts UI and composer autocomplete.
- **Roadmap tracking**: the Phase 4 Contacts UI checklist in `TODO.md` (split layout, category model, selection wiring, right-side list, and details dialog) is now marked complete to match the implemented behavior.

## 0.6.39 (2026-08-06)

### Added

- **Contacts/GOA**: GOA Contacts discovery and credential plumbing are now first-class alongside Mail and Calendar. `lookout-goa` gained `list_contacts_accounts()` plus Contacts-specific auth modeling (`ContactsAuthMethod`) and credential helpers (`ensure_credentials_contacts`, `get_access_token_contacts`, `get_contacts_password`), with the fake GOA D-Bus integration test extended to cover Contacts interface discovery and both OAuth2/password credential paths.
- **Contacts/CardDAV**: the app now performs CardDAV contacts sync on startup for GOA contacts-enabled accounts and refreshes it periodically in the background (15-minute poll). Contacts are fetched through `lookout-dav` (`discover_addressbook_home` → `list_addressbooks` → vCard fetch/parse), normalized to email-address suggestions, deduplicated by address, and stored in UI state per account.

### Changed

- **Compose**: recipient autocomplete now merges two sources for the active account: (1) correspondence-ranked local mail-history addresses from the SQLite cache and (2) CardDAV-derived contacts, both filtered by the current prefix and deduplicated before display. This keeps the keystroke path synchronous and low-latency while materially improving completion coverage beyond previously seen correspondents.
- **Core API**: shared contacts lookup was formalized with `lookout-core::ContactsProvider`; `lookout-mail::Cache` now implements it, and composer suggestion call sites use the trait-facing `search_contacts` surface.
- **Core/vCard**: `lookout-core`'s RFC 6350 implementation gained `VCard::email_addresses()` to project parsed vCards into shared `EmailAddress` values (with display-name derivation), and tests were expanded accordingly.

### Fixed

- **Build hygiene**: the previous dead-code warning suppressions around staged CardDAV helpers were removed by wiring those paths into real app usage. Workspace checks now complete cleanly (`cargo check -q`) with the CardDAV/contact code active rather than lint-silenced.

## 0.6.38 (2026-08-05)

### Changed

- **Mail**: three proportions on the mail screen. The message list starts 15% wider (the `Paned` position between it and the reading pane goes from 320px to 368px); since that `Paned` gives the extra space to the reading pane on resize (`resize_end_child(true)`), the 48px comes out of the reading pane rather than the folder pane. The gap between the message list and the reading pane is halved, 24px to 12px: that gap is the two cards' facing margins plus the separator between them, and it's the margins that are dropped to zero rather than the separator that's narrowed, so the full 12px stays draggable - the separator is transparent, so the two approaches look identical but only the separator grabs the pointer. The folder pane now has a 150px minimum width, so the separator can't be dragged left until the folder names are a sliver. That floor goes on the folder pane's card, not on the `Gtk.ScrolledWindow` inside it: a `ScrolledWindow` deliberately absorbs its child's size request instead of propagating it, which is why the pane had no meaningful minimum in the first place, and it only bites at all because the `Paned` sets `shrink_start_child(false)`.

## 0.6.37 (2026-08-05)

### Added

- **Mail**: the folder pane shows unread counts, right-aligned at each row's trailing edge rather than parenthesised after the folder name - the name takes the row's slack (and ellipsizes into it on a narrow pane) so the numbers line up in a column, drawn in the message list's own accent blue with tabular figures so they don't jitter as they update, and hidden entirely at zero rather than blanked, which would leave every quiet folder's name ending short of its neighbours. Counts come from a per-folder `STATUS (MESSAGES UNSEEN UIDNEXT UIDVALIDITY)` pass; `LIST` reports no counts at all, so they're queued after the folder tree's first paint rather than gating it, and a folder whose `STATUS` fails keeps its zeros rather than costing the session its connection. The "All Inboxes" row and each account row carry a count too: the sum of every account's Inbox, and that account's Inbox respectively - the Inbox rather than a sum over all folders, or a Junk folder with four thousand unread would be all the number ever said. A `\Seen` store patches the open folder's count immediately instead of waiting for the next pass, and an IDLE notification re-`STATUS`es the folder the new mail landed in.

### Fixed

- **Mail**: switching folders no longer stalls for a second or two while unread counts are being collected. Three separate causes, all of them the count pass colliding with the interactive session. (1) The main loop entered IDLE unconditionally at the top of every iteration - so a command that arrived while the session was busy with background work paid a `SELECT`, an IDLE init and an IDLE `DONE`, three round trips, purely to be handed something already sitting in the queue. A command already queued is now taken with `try_recv` and handled without touching IDLE at all. (2) The count drain ran a few `STATUS` calls per IDLE cycle against a 50 ms IDLE slice, spending two round trips re-establishing and tearing down IDLE for every three useful ones and keeping the session busy for the whole pass - exactly what a folder click had to queue behind. It now runs to completion in a single iteration (a `STATUS` is one round trip; a whole pass is cheap), yielding to the command queue before every round trip, so the longest a user action ever waits is one in-flight `STATUS` and the queue resumes where it stopped. The drain also runs *ahead* of the background body prefetch, since counts are one round trip each and immediately visible while prefetch is minutes of bulk downloading. (3) The drain worked backwards through the folder list, so the folder the user was actually looking at got its count last; it now goes open folder, then Inbox, then the rest.
- **Mail**: the folder pane no longer jumps to "All Inboxes", loses its highlight, or collapses expanded subfolders while counts arrive. `rebuild_folder_tree` swaps the `SingleSelection`'s model, and `GtkSingleSelection`'s autoselect lands on row 0 - the "All Inboxes" row - firing the `selected-item` handler as though the user had clicked it. That was survivable when `FoldersUpdated` only fired on connect, Refresh and message moves; with per-folder counts it fires repeatedly and would yank the view out from under a folder switch in progress. The rebuild now notes the selection and the expanded rows first and puts both back afterwards, with the selection handler suppressed for the restore so it isn't mistaken for navigation. Most of those events change nothing the sidebar draws, so a signature of exactly what the tree renders (per folder: id, name, role, delimiter, unread - deliberately not `uidnext`/`uidvalidity`, which a `STATUS` rewrites on every pass) short-circuits the rebuild entirely for them.
- **Mail**: folder counts no longer blank and refill on every launch, Refresh, or message move. A `LIST` reports no counts, so each re-list arrived all-zero and overwrote what had already been learned; counts are now carried across a re-list by mailbox id, and seeded from the on-disk cache on connect, so the sidebar opens with last session's numbers while the fresh pass runs behind them.

## 0.6.36 (2026-08-05)

### Fixed

- **Calendar**: the large month grid no longer swallows events that span more than one day. Occurrences were bucketed by their start date only, so a multi-day event appeared in just its first day's cell - and an event that *started* before the displayed 6-week window (a trip from the previous month, say) vanished entirely, since its start date isn't a cell in the grid. The grid now buckets by every date an occurrence covers, using the same column maths as the Day/Week/Work week chips (`occurrence_day_range`): a 3-day trip renders a chip on each of its days, an event ending exactly at local midnight still stops on its last real day rather than leaking into the next, and an event starting before the window shows on all of its in-grid days. The same bug class is fixed in the two places that share the occurrence list: the sidebar mini-calendar's bold event-day markers (`calendar_event_days`) and the Mail screen's overview-pane day list (`refresh_mail_overview_day_list`) now both count every covered day, so a multi-day event bolds each of its days and appears in the overview on every day it runs. Both route through a shared `covered_local_dates` helper so the three views stay in agreement.

## 0.6.35 (2026-08-05)

### Added

- **Mail**: the message-list header's Filter button is no longer a disabled placeholder - it's a live `MenuButton` with radio items (All / Unread / Flagged) bound to a `win.list-filter` stateful action, mirroring the sort-key menu beside it. The active filter is applied inside `MessageListModel::repopulate`, the single choke point every list rebuild passes through, but against a new *unfiltered* source of truth: `repopulate` stashes the full message set in the model before filtering, so the `displayed` snapshot stays the filtered subset the next sync's no-op-rebuild check diffs against while `set_filter` (and the sort controls, which now read `MessageListModel::all_messages`) re-render from the full set. `Unread`↔`All` therefore round-trips without losing a message, a flag flip on a message that's filtered out is seen as no change (a `STORE` on it costs no rebuild), and the filter persists across folder switches and view changes for the session.

## 0.6.34 (2026-08-05)

### Added

- **Compose**: recipients are chips rather than a run of comma-separated text. To and Cc (and a new Bcc field - `ComposedMessage.bcc` was already carried end to end through the send path, the draft autosave, and the MIME builder; only the UI field was missing) are `RecipientEntry` widgets: each recipient is a removable pill in a wrapping `Gtk.FlowBox`, with the text entry trailing the last chip so typing always continues where you left off. Enter, comma, semicolon, or Tab commits what's typed; the chip's × or Backspace on an empty entry removes one; pasted text splits into several at once. Tokenizing respects quoted display names, so `"Lovelace, Ada" <ada@example.com>` stays one recipient instead of two broken ones - the naive `split(',')` the Send handler and the draft autosave each did separately got that wrong, and both now route through the same tokenizer. A chip whose contents don't parse as an address is styled as a warning rather than refused: the server is the real authority on whether an address exists, and silently rejecting an unusual-but-valid address is worse than a chip that looks wrong and bounces. Send commits any half-typed token first, so a recipient can't be dropped for want of pressing Enter, and `Bcc` is part of the draft snapshot, so a blind-copy-only composer still autosaves.
- **Compose**: typing a recipient offers completions from an address book harvested out of synced mail. There is no contacts source to draw on until Phase 4's CardDAV work, so a new `addresses` table in each account's SQLite cache records every `From`/`To`/`Cc` address seen (`Cache::record_addresses`, called from `emit_messages` - the choke point every synced envelope set already passes through, and deliberately not inside `replace_messages`, which wipes and rewrites a mailbox's window on each sync while the address book has to accumulate across them). Addresses are keyed lowercased so one correspondent isn't two entries, and a display name once learned survives a later envelope carrying only a bare address. Completions match a prefix against the address and the name both, ranked by how often each correspondent has appeared, and are filtered against what's already chipped. The lookup runs synchronously on the UI thread against a second, read-side cache handle rather than going through an `AccountCommand`: a keystroke can't wait behind whatever IMAP round trip the session is mid-way through. `Cache::open` now sets `journal_mode=WAL` and a busy timeout so that second reader and the session's writes coexist instead of colliding as `SQLITE_BUSY`; a failed query is silently no suggestions, never an error to dismiss mid-sentence. The table fills as mailboxes sync under this build - existing cached mail is not backfilled, so the first composer after upgrading has little to offer until some folders have re-synced.

## 0.6.33 (2026-08-05)

### Fixed

- **Compose**: opening a composer in rich-text mode (the default) aborted the whole app about five seconds later - `called Result::unwrap() on an Err value: EnterError` from glib's `main_context_futures.rs`, followed by `panic in a function that cannot unwind`. The cause was `read_content`, which reads the contenteditable editor back out of WebKit: it wrapped `evaluate_javascript_future` in `glib::MainContext::block_on` to keep its callers synchronous, and `block_on` runs a *nested* main loop. That nested loop dispatches another `TaskSource`, whose `futures_executor::enter()` fails because the thread already has an executor entered for the outer dispatch; glib unwraps that `Err`, and `TaskSource::dispatch` is an `extern "C"` callback that cannot unwind, so the panic becomes an abort. The nested loop was harmless as long as the only caller was the Send click handler (a GTK signal callback is not inside a `TaskSource` dispatch), which is why rich-text send has worked since 0.6.28 - but draft autosave, added in 0.6.32, calls the same function from a `glib::spawn_future_local` timer every five seconds, which is. `read_content` is now `async` and awaits the evaluation on the executor that's already running, so no nested loop is ever created. Its callers changed shape to suit: the `Rc<dyn Fn()>` autosave closures became an `AutosaveCtx` struct with `async` methods (a closure can't hold an `async` body), and Cancel and Send each run their remaining work in a spawned task. Send now sets `closed`/`on_done` at the *end* of that task rather than before it, so the reading pane can't close out from under the editor read - the same latent abort from the other direction, since Send's nested loop would have dispatched the autosave task, which would have called `block_on` from inside a dispatch.
- **Compose**: two draft autosaves could overlap and each `APPEND` a copy, leaving two messages in Drafts under one `Message-ID`. Both the timer tick and Cancel can start a save, and each now awaits a WebKit round trip in the middle, so both could observe `draft_queued == false` and neither would ask the server to replace the other's copy. A single in-flight guard on `AutosaveCtx` makes a save that starts while one is running return immediately.
- **Compose**: a composer displaced without Cancel or Send left its five-second autosave timer running forever against a detached editor, still writing drafts. The message row's hover quick-action Reply was the way to reach that: it built its composer inline with an empty `on_done`, and added a second `"compose"` page without removing the one already there, so that composer could never close. It now routes through `show_composer_in_reading_pane` like every other Reply/Forward/New Message entry point, which replaces any existing composer, restores the previously visible page on close, and owns the draft-confirmation relay. As a backstop, the composer also stops its timer when its widget leaves the window tree.

## 0.6.32 (2026-08-05)

### Added

- **Mail**: messages can now be marked read and flagged. Neither was possible before: the only `STORE` in the codebase was the `\Deleted` one inside the move fallback, so opening a message left it unread forever (bodies are fetched with `BODY.PEEK[]` precisely so the client decides when `\Seen` is set - but nothing ever set it), and the toolbar's Flag button was a disabled placeholder. A new `AccountCommand::StoreFlags { mailbox, uid, add, remove }` issues the add and remove halves as two separate `+FLAGS.SILENT`/`-FLAGS.SILENT` stores (IMAP has no combined form; an empty side is skipped rather than sent as an empty flag list, which servers may reject). `.SILENT` because the caller already knows the resulting flag set and the next sync re-reads the real flags anyway. Opening a message in the reading pane marks it `\Seen` - on open, as Outlook's default reading pane does; Bulwark's configurable mark-as-read delay is a later refinement. The Flag button is live and toggles `\Flagged` in whichever direction the selected row's own flags call for, and flagged rows draw an amber marker between the subject and date columns, hidden entirely (not merely blank) when unflagged so it costs no width on ordinary rows. Unlike `FetchBody`, which drops any request for a folder other than the open one, `StoreFlags` selects the message's own folder when they differ - a mark-as-read can race a folder switch, and the unified "All Inboxes" view mixes mailboxes by construction - and the main loop's existing pre-IDLE re-select puts the session back on the user's folder afterwards. A successful store patches the cached summary in place (`Cache::update_flags`) and re-emits the mailbox from cache instead of re-syncing, so a mark-as-read costs one `STORE` and no fetch, and a restart before the next sync doesn't show the message unread again; if the uid falls outside the cached window (or there's no cache at all) it falls back to a full re-sync. `message_row_key` gained the flagged bit alongside the unread one, so a flag-only change is seen as a real change rather than swallowed by the message list's no-op-rebuild check.

## 0.6.31 (2026-08-05)

### Fixed

- **Config**: the window-background picker added in 0.6.30 did not compile as written. Both signal closures moved `config_view` into themselves while the `connect_activated` call still borrowed the row widget off it as the receiver, and the picker closure additionally carried the borrowed `&ActionRow` parameter into the spawned file-chooser future, which needs `'static`. The widgets are now cloned before connecting - each handler captures only the two rows it actually touches plus the background `Picture`, never the whole `ConfigView` - and the row is cloned again for the async block.
- **Testing**: the `background_image` round-trip test was flaky. Both of the module's tests set `XDG_CONFIG_HOME` - a process-global env var - and Rust runs tests on parallel threads, so the stale-path test could rewrite the variable while the round-trip test sat between its `save` and `load`, making the load read the wrong directory. A single test now owns the env var and covers both the save/load/clear round trip and the stale-path fallback.
- **Build**: the workspace is clippy-clean again under the current stable toolchain, whose new lints had pushed eleven pre-existing spots past CI's `-D warnings` gate. The mail session's prefetch path swaps `len() > 0` / `len() == 0` for `is_empty()` and `map_or(false, …)` for `is_some_and(…)`; the calendar-colours test's `format!("{id}")` becomes `to_string()`; the calendar style-context probe builds its C string with a `c"probe"` literal instead of a hand-rolled NUL-terminated byte string; `rebuild_folder_tree`'s snapshot tuple is factored into a `FolderTreeSnapshot` type alias instead of an inline "very complex type" annotation; and the `window.rs` test module moved to the end of the file to satisfy the items-after-test-module lint. Behavior is unchanged throughout - pure lint hygiene.

## 0.6.30 (2026-08-05)

### Added

- **Config**: the Config view's "Appearance" section can now replace the window's background artwork. A "Window background" row opens a file chooser filtered to whatever formats `GdkPixbuf` can decode; the chosen image replaces the bundled artwork behind every pane immediately, and the row's subtitle names the file in use. A second "Restore default background" row - only active while a custom image is set - switches back to the bundled artwork. The choice persists across restarts as a plain path in `$XDG_CONFIG_HOME/lookout/background-image-path` (the same best-effort file convention as calendar colours, until Phase 5's GSettings lands): it's re-applied at startup, and a stored path whose file has since been deleted, moved, or become unreadable falls back to the bundled artwork silently rather than blocking the window or erroring at launch.

## 0.6.29 (2026-08-05)

### Fixed

- **Mail**: Microsoft 365 accounts (the "Microsoft 365" entry GOA's Online Accounts creates for Microsoft work/school and personal accounts) now actually work. Two separate problems blocked them. First, they silently vanished from the account sidebar: GOA's `ms_graph`/`microsoft365`/`microsoft` providers model mail as EWS/Graph, so their `Mail` interface reports `ImapSupported`/`SmtpSupported` false and leaves every IMAP/SMTP host, port, and username empty - which account discovery read as "no usable mail" and filtered out. Discovery now recognizes those provider types and, instead of discarding the account, supplies the known-good Exchange Online settings itself: `outlook.office365.com:993` for IMAP, `smtp.office365.com:587` for SMTP, TLS on both, and the account's own email address as the username. Second, even once listed, the account couldn't authenticate: GOA's OAuth2 token for Microsoft 365 accounts carries only Microsoft Graph scopes (`mail.readwrite`, `user.read`, ...) - there is no `https://outlook.office.com/IMAP.AccessAsUser.All` / `SMTP.Send` in GOA's provider config at all - and Exchange Online's IMAP/SMTP endpoints reject that token (`NO AUTHENTICATE failed`, verified live). Microsoft accounts therefore bypass GOA for credentials entirely: the app runs its own public-client OAuth2 authorization-code flow (`microsoft_oauth.rs`) against Microsoft's v2.0 endpoints with PKCE and a loopback redirect, requesting the two outlook.office.com scopes plus `offline_access`. The first connect opens a browser to sign in and consent the app's permissions; the refresh token is then stored per account under `$XDG_DATA_HOME/lookout/oauth/` with 0600 permissions, and every later connect exchanges it silently for a fresh access token, cached in memory until near expiry. If the stored refresh token is ever rejected (revoked, expired, or issued for a different client id), the app drops it and runs the interactive flow again rather than getting stuck. The flow uses Lookout's own Entra app registration (client id `07725d7c-f588-41ae-bdd0-7ee625fed328`, public client with the `http://localhost` loopback redirect and the `IMAP.AccessAsUser.All`/`SMTP.Send` delegated permissions), so the consent screen presents Lookout by name. Non-Microsoft accounts still require GOA to advertise IMAP/SMTP support and still authenticate through GOA as before.

- **Notifications**: connection failures no longer spam the toast banner. Mail and calendar sessions already retry failed connections themselves with backoff, yet every attempt fired two toasts with the same message (a duplicate `Error` event plus the connection-state event). Retryable failures are now treated as warning-level - logged and (for calendars) still shown in the account's sidebar status, but silent in the toast overlay; only non-retryable (fatal) errors pop a toast. Explicit action failures (send, move, clearing caches) are unchanged and still surface.

## 0.6.28 (2026-08-05)

### Added

- **Compose**: the composer's body is no longer plain-text only - a "Rich text" switch above the body flips between the existing `Gtk.TextView` and a contenteditable WebKit `WebView` with a formatting toolbar (bold / italic / underline / strikethrough, bulleted and numbered lists, font size, text color, and link insertion). Sending in rich mode emits a `multipart/alternative` message - both the formatted HTML and a plain-text rendering - via `mail_builder`, so HTML-capable clients get the rich version and everything else falls back to the text. Reply/Reply-All/Forward keep their existing plain-text prefills, converted to simple HTML (escaped, `>` quotes become blockquotes, `---` lines become horizontal rules) so both modes start from identical content. Only one mode is live at a time. The editor vetoes navigation and all remote subresources in its `connect_decide_policy` handler (compose must never fetch remote content, regardless of the reading pane's "Load images" setting), and reads its content back out with a single `evaluate_javascript` round trip that also normalizes WebKit's `<font>` wrappers from the size/color commands into styled spans; if that read fails it falls back to the prefill body so a Send click can never drop the message.
- **Config**: the Config view's "Mail" section now also has a "Rich text" switch, next to "Load images from the web". On by default, it sets which body mode new composers open in - the formatted WebKit editor or the plain-text fallback - for New Message, Reply, Reply-All, and Forward alike. Like the other preferences it's session-only until Phase 5's GSettings lands, and it's read when a composer opens, so an already-open draft is never switched underneath the user.

## 0.6.27 (2026-08-05)

### Added

- **UI**: the message-list pane now has a secondary header between the pane's main header (folder name over account, plus the sort/sync controls) and the message rows, naming the columns below it - Sender / Subject / Date. It mirrors each row's internal geometry rather than approximating it: a gutter where the unread accent bar and avatar sit, then the fixed-width sender column, the expanding subject column, and the right-aligned date - so each title sits exactly over the data it names instead of drifting as the pane resizes. A new `.message-column-header` CSS class styles it as a subtle dim band with hairline top/bottom borders, visually separating the list's title row from its controls above and its content below.
- **Config**: the Config view's "Mail" section (previously a disabled placeholder) is now live with a "Load images from the web" switch. Off by default, it controls whether the reading pane's WebView may load images embedded in emails that are hosted on remote servers. The existing `connect_decide_policy` subresource veto - which blocked every non-local response (tracker pixels, remote images/fonts, `<iframe>`s) so a slow or broken external host couldn't hold the pane's HTML reveal hostage - now lets `image/*` responses through when the switch is on; everything else (scripts, fonts, iframes) stays blocked, the navigation veto is unchanged, and JavaScript is still disabled outright. The preference lives in `UiState::load_remote_images` - the single source of truth the switch flips - and is re-read on every resource decision, so the content viewer always reflects the current setting. Flipping it re-renders whichever message is open so the change takes effect immediately rather than on the next selection (skipped while a composer is up in the reading pane, which `render_body` would otherwise displace). Session-only until Phase 5's GSettings lands, matching the other preferences.

## 0.6.26 (2026-08-05)

### Fixed

- **Mail**: a folder could silently stay empty - the message list never populated - when a `SyncMailbox` request arrived queued behind another command (e.g. you clicked a message and then quickly a folder). The pre-IDLE cached emit only fired when `SyncMailbox` was the command that *woke* the session; when it was processed out of the drain queue with a cache hit, the cache-skip path did `continue` without emitting anything or running a live sync. The app's pending-sync entry for that folder was then never cleared, and it suppressed every later sync request for the folder until the next reconnect. The `SyncMailbox` handler now emits the cached message list itself on a cache hit (via a shared `emit_cached_messages` helper), so the list populates and the pending-sync entry clears regardless of how the command arrived.
- **Mail**: IDLE could end up monitoring the wrong folder after a cache-served folder switch. The cache-skip path serves the switch without a `SELECT`, leaving the session selected on the old folder; IDLE only reports changes to the *selected* folder, so new mail in the folder the user was actually viewing never triggered a resync. The session now tracks which mailbox it is actually selected on (`session_selected`) and re-selects the user's current folder before re-entering IDLE whenever they've drifted apart - a cheap round trip (no envelope fetch), skipped whenever the session already matches.
- **Mail**: the background body prefetch issued two `SELECT`s back-to-back on its first visit to a mailbox - one whose result was discarded and one real one - wasting a full IMAP round trip per mailbox. The duplicate is gone.
- **Performance**: the background body prefetch is now genuinely interruptible instead of just cooperative in name. It already yielded between IDLE cycles, but once a batch started it could hold the session through the `SELECT`, the envelope-UID fetch, and up to `PREFETCH_BATCH_SIZE` body downloads while user commands sat in the queue. It now checks for pending commands before the `SELECT`, before the envelope fetch, and before each body fetch, returning the un-downloaded UIDs to the queue and breaking out so the user's action is processed promptly. The final re-select of the user's folder is also skipped when a command is pending (the command handler selects the folder it needs, and the new top-of-loop check re-selects the user's folder anyway).

## 0.6.25 (2026-08-04)

### Fixed

- **Performance**: the envelope cache emit for folder switches now fires the instant the `SyncMailbox` command arrives, *before* the IMAP IDLE teardown (`DONE` round-trip), rather than inside `sync_mailbox()` where it had to wait for the network exchange to complete. The cached message list now paints on screen while the server round-trip is still in flight, cutting the perceived folder-switch latency roughly in half.

## 0.6.24 (2026-08-04)

### Fixed

- **Performance**: folder switches now show the cached message list instantly from the on-disk SQLite envelope cache before the IMAP re-select completes. The existing `messages` table (populated by `replace_messages` after each sync) is loaded and emitted as a `MessagesUpdated` event at the top of `sync_mailbox`, before the IMAP `SELECT`; the live fetch then runs in background and emits a second update with fresh data. First visits to a folder still wait for the IMAP round-trip (the cache is empty), but all subsequent visits within the same session or across restarts are instant.

## 0.6.23 (2026-08-04)

### Fixed

- **Performance**: message body switching no longer re-fetches over IMAP for every selection. A disk-backed body cache (per-mailbox row cap with LRU eviction) serves previously seen messages instantly, and a small in-memory LRU cache (25 entries) avoids even the disk read for recently viewed messages.
- **WebKit remote subresource blocking**: the reading pane now blocks non-local URI loads (remote images, scripts, etc.) via `connect_decide_policy`, preventing slow and broken subresource fetches from gating the HTML reveal. A 400ms fallback reveal ensures the pane shows even if a load stalls.
- **Crash fix**: fixed a `RefCell` double-borrow panic in the body cache lookup path where a `RefMut` guard outlived a `render_body` call.

### Added

- **Tracing**: debug-level instrumentation across the body fetch and render pipeline (`FetchBody dispatch`, disk/in-memory cache hits, UI thread arrival, `Finished` reveal timing).

## 0.6.22 (2026-08-04)

### Changed

- **UI**: the Mail screen's right-hand calendar overview pane now renders its mini month grid at roughly half its previous width. The pane has always carried a `width_request(140)`, but a `width_request` is only a floor - the grid's seven day buttons were each asking for Adwaita's default button metrics (16px min-width plus 10px of horizontal padding either side, so ~36px a cell), which made the grid's *natural* width ~260px and that, not the request, is what the pane actually rendered at. A new `.mini-calendar-compact` CSS class (applied only to the Mail pane's mini calendar, not the Calendar view's own 240px sidebar copy) drops the day buttons to `min-width: 0` with 2px of horizontal padding and sizes their labels at 0.8em, taking each cell to ~20px so seven columns finally fit inside the requested width.

## 0.6.21 (2026-08-04)

### Added

- **UI**: message-list rows are now the Outlook-style layout the reference screenshot shows. Each row is a colored initials avatar, then a three-column line - sender, subject, right-aligned date - over a dimmed one-line preview of the body. Unread rows take an accent-blue sender/subject/date plus a blue bar flush against the pane's leading edge; read rows use a muted slate sender and a warmer subject tone. The sender column is fixed-width and ellipsized rather than `hexpand`ing, which is what lines every row's subject up in one column instead of starting it wherever the sender happens to end; the subject takes the `hexpand` instead. The avatar reuses the reading-pane header's existing `initials`/`avatar_color_class` helpers (now `pub`) and its `.avatar-circle`/`.avatar-color-*` palette, so a sender keeps the same color in both places. The preview line is always present, empty string and all, so read and unread rows stay the same height and the list doesn't ripple as previews arrive.
- **UI**: the message list now groups by date under collapsible section headers - Later / Today / Yesterday / This Week / Last Week / This Month / the previous month by name ("July") / Older - each with a disclosure chevron, built on `Gtk.TreeListModel` and `Gtk.TreeExpander` (the same pattern the folder tree already uses). Sections are expanded by default, including ones that first appear in a later rebuild; only an explicit collapse is remembered, and it survives the constant list rebuilds. Bucketing compares *calendar dates in local time* rather than elapsed durations, so an 11pm message reads as "Yesterday" rather than "Today" just because it's inside 24 hours, and the clause order puts the 1st of the month in "This Week" rather than "This Month". The named-month bucket is deliberately last month only - matching Outlook, where a July message sits under "July" but a February one from the same year is already "Older". `DateBucket` is payload-free (no `Month(year, month)` variant) precisely because the collapsed-section set is keyed by it, and a dated variant would orphan that state every time the calendar rolled over.
- **Mail**: message previews are real. `EmailSummary::preview` had always been hardcoded `None` - an envelope-only fetch can't produce one - so `sync_mailbox` now runs in two phases: the existing `ENVELOPE` fetch paints the list at exactly today's latency, then a second pass fetches `BODY.PEEK[]<0.16384>` for up to 50 previewless messages, extracts a single display line, and emits a second `MessagesUpdated`. A byte-prefix fetch needs no `BODYSTRUCTURE` round trip and no part-number guessing, and `PEEK` can't set `\Seen`. The window is generous because what's bounded is the *raw* prefix, not the readable text in it - a marketing HTML mail can spend its first several KB on headers and a `<style>` block before any prose. Snippet extraction strips zero-width characters first (bulk senders pad the top of a message with runs of them as preheader spacing, and left in place the preview renders as an apparently blank line), collapses whitespace, and truncates on a character boundary. The whole second phase is failure-swallowing: `sync_mailbox`'s caller tears the connection down on `Err`, and a malformed message or a server that dislikes partial fetches must not cost the user their IMAP session over a cosmetic snippet.
- **Mail**: cached previews are carried forward across resyncs (`Cache::load_previews`). `sync_mailbox` is a full bounded re-fetch on every IDLE wake, not a delta, and `replace_messages` wipes the mailbox's rows each time - so without reading snippets back *before* that wipe, every resync would blank the whole list and re-fetch every body. Needs no schema migration: the cache already stores each `EmailSummary` as JSON, so rows written before previews existed simply deserialize with `preview: None` and get backfilled on the next sync.

### Changed

- **UI**: the message list's model moved from a flat `gio::ListStore` of envelopes to a two-level `Gtk.TreeListModel`, and the model layer moved out of `window.rs` into a new `message_list.rs` (with `SortKey` and `sort_messages`). `repopulate_message_list`/`snapshot_message_store`/`store_matches` are gone, folded into `MessageListModel::repopulate` and `displayed_messages`. Every existing optimization is preserved: the rebuild-skipping identity check, selection survival across a `splice`, and the reading pane's `rendered_message` crossfade guard. The check now compares the *sort* alongside the contents, because a list can be element-identical under ascending and descending order yet still need re-grouping - the sections come out mirrored. `message_row_key` gained `preview`, which is load-bearing rather than cosmetic: the two-phase sync's second event differs from the first in nothing else, so without it the identity check would skip the rebuild and no snippet would ever reach the screen.
- **UI**: selecting a section header is deliberately a no-op in the reading pane rather than clearing it. `GtkSingleSelection` autoselects row 0 after every rebuild, and in a grouped list row 0 *is* a header - so clearing there would yank the pane to "empty" and reset `rendered_message` on every cache replay and live sync, resurrecting exactly the startup-flicker bug 0.6.19 fixed. Collapsing the section holding the selected message lands in the same branch, and keeping the message on screen is right there too. Header rows are also non-selectable by mouse, but that alone is not sufficient - it doesn't stop the programmatic autoselect, so both defences are required.
- **UI**: list dates are no longer locale-formatted. `%X`-within-24h / `%x`-otherwise became an explicit two-tier weekday-and-time for the last week ("Mon 10:20 PM") and a numeric date beyond it ("15/07/2026", "4/12/2025" - no leading zero on the day). The recent tier widened from 24 hours to 7 days, since a weekday abbreviation is only unambiguous within a week.
- **UI**: the message list never scrolls sideways (`hscrollbar_policy: Never`). The preview label deliberately holds far more text than fits so the snippet runs to the pane's edge at any width and re-flows on resize; without pinning the policy its natural width request would let the list report a huge natural width and grow a horizontal scrollbar.
- **UI**: section expansion state is read back off the model immediately before each rebuild rather than tracked through `notify::expanded`. The signal approach was tried first and does not work, in both directions: `TreeListModel` hands out `TreeListRow`s on demand and doesn't retain them, so a handler connected during a rebuild is dropped with its row and never sees the user's later collapse - and the same signal *also* fires when a splice tears rows down, which is indistinguishable from a real collapse. Together those meant a rebuild recorded phantom collapses of sections nobody touched while silently losing the real ones. Reading the model's own expansion state has neither problem. A flat (sender/subject) sort is explicitly not allowed to write the collapsed set, since it has no section rows and would otherwise forget the user's collapses across a there-and-back sort change.

### Fixed

- **UI**: message-row hover and quick-action handlers no longer accumulate on recycled rows. `connect_bind` attached a fresh `EventControllerMotion` and three fresh `connect_clicked` handlers on every rebind while `connect_unbind` was an empty stub, so scrolling a long list stacked handlers on the same reused widgets and a single click could fire against several messages at once. All four are now connected once in `connect_setup`; the row's current message lives in a cell that `bind` writes and the handlers read *at click time*, which is what makes one-time connection possible.

### Added

- **UI**: the message-list pane now has a real header naming what you're looking at. The left side shows the open folder's name over a dimmed line with its owning account ("Inbox" / "gavindi@gmail.com"), falling back to the mailbox id's path segment while an account is still connecting and reading "All Inboxes" / "All accounts" in the unified view. To its right sits the list's own control cluster - favorite star, Sync, Filter, sort direction, and a "By Date" sort-key dropdown - pushed to the trailing edge by the title column's `hexpand` rather than a spacer widget. Because the header's icon names (`starred-symbolic`, `funnel-symbolic`, `view-sort-*-symbolic`, `view-refresh-symbolic`) are ones this codebase hadn't used before, each resolves through a new `themed_icon_name` helper that checks `Gtk.IconTheme::has_icon` and falls back to a name already proven in-tree - the app renders no missing-image boxes on a theme that lacks them.
- **UI**: message-list sorting is now live. The sort-key dropdown offers By Date / By Sender / By Subject (a stateful `win.sort-key` action, so the menu shows real radio checks), and the direction toggle flips between newest-first and oldest-first, swapping its own icon and tooltip. Sorting is applied in `repopulate_message_list` - the single choke point every list rebuild passes through - so the order is uniform no matter which event produced the rebuild, and it happens *before* the "nothing changed" identity check, which means a sort change is itself detected as a real change with no extra plumbing. Every key tie-breaks on date so the order is total: two messages with the same sender or subject can't shuffle between otherwise-identical rebuilds and defeat the rebuild-skipping optimization. Ascending is the descending order reversed rather than a second set of comparators, so "oldest first" is the exact mirror of what was on screen. Re-sorting reads the visible list back out of the store (`snapshot_message_store`) instead of re-fetching - single-mailbox views keep no snapshot in `UiState`, only the unified view does - and routes through the existing selection-restore path, so the selected message stays selected and the reading pane doesn't re-render (or re-crossfade) the email it's already showing.
- **UI**: the Sync button re-syncs whatever the list is currently showing - the open mailbox, or every connected account's Inbox in the unified view - reusing the existing `request_mailbox_sync`. That dedupes against in-flight requests, so pressing Sync while a sync is already outstanding is deliberately a no-op rather than a second round trip.
- **UI**: the header's star adds the open folder to a new "Favorites" section pinned to the top of the folder tree (between "All Inboxes" and the account groups, omitted entirely when nothing is starred). Favorites render as flat leaves - a favorite is one folder, not a subtree - and selecting one behaves exactly like selecting the folder's real row. The star is insensitive in the unified view, which spans every account and so has no single folder to favorite. Session-only until GSettings lands, matching the View tab's layout toggles. Because a starred mailbox now has *two* rows in the tree, `find_mailbox_index` deliberately matches only `TreeItem::Folder` and never the `Favorite` duplicate - the favorites section sorts above the account groups, so matching both would resolve every favorite to its copy and break startup view restore.

### Changed

- **UI**: the message-list pane's account-switcher dropdown is gone, along with `refresh_account_switcher`, `find_account_inbox_index`, `sorted_account_entries`, and the three switcher widgets that were threaded through `spawn_account_discovery`, `connect_account`, and `rebuild_folder_tree`. It duplicated navigation the folder tree already provides while telling the user nothing about what was on screen; the folder tree (including its "All Inboxes" row) is now the only place to switch accounts, and the space it occupied is the new header. The signal handlers that used to re-sync the dropdown now refresh the list header instead, at the same choke points.
- **UI**: the header's Filter button ships visible but disabled, following the codebase's honest-disabled convention (as with the command toolbar's Flag button) - a real filter needs unread/flagged plumbing and new `AccountCommand`s that don't exist yet.

## 0.6.19 (2026-08-04)

### Fixed

- **UI**: the reading pane no longer crossfades the same email in and out several times in a row on startup. The startup burst - each account's SQLite-cache replay plus its live sync plus the app's on-demand syncs - delivers the same envelope set to the message list up to six times in quick succession, and each one used to trigger a full list rebuild. Rebuilding reset the selection, which re-selected the first row and re-rendered its already-displayed body, routing the reading pane through its "empty" placeholder and crossfading the same email again (two accounts × three duplicate events ≈ six fades). Three layered fixes: (1) `repopulate_message_list` now compares the incoming envelopes against what's already displayed and skips the rebuild entirely when the list is identical, which is the normal case for a duplicate; (2) the selection handler recognizes a re-selection of the exact message already on screen and returns early instead of re-rendering it; and (3) duplicate `SyncMailbox` requests are deduped per account until the earlier one is answered, so the app's own on-demand syncs stop piling onto the cache-replay ones. The remaining fades came from the rebuild itself: it ran as `remove_all` + `append`, which momentarily emptied the model, dropped the selection, and fired the handler's empty branch - clearing the same-message guard so the re-selected email got re-rendered (and re-faded) one more time. That is now a single atomic in-place `splice` that preserves the selection position, so the guard survives rebuilds. Startup now settles to one fade-in of the actual newest message instead of fading the same email in and out.

## 0.6.18 (2026-08-04)

### Changed

- **UI**: the reading pane now crossfades (100ms `Crossfade` transition) between its pages instead of snapping, so switching messages fades the old body out and the new body in. The message header (subject/avatar/recipients) now lives *inside* the crossfading "message" page alongside the body - grouped with the body's web/text views in a no-transition inner stack - so the header and body fade out and in together instead of the header popping out of sync with the content. Message switches already pass through the pane's "empty" placeholder, so both halves of the fade fire for free; the HTML page is held on the placeholder until WebKit actually finishes loading the new body (`load-changed` Finished) so the fade-in never reveals a still-blank white page. Same-page re-renders (e.g. the debug `.eml` opener loading into an already-visible page) route through "empty" explicitly to force the transition.
- **UI**: the Config view's "Appearance" section is now live (it was a disabled placeholder) with an "Animate transitions" switch that disables the reading pane's crossfades. It's the first real preference in the Phase 5 settings taxonomy and, like the layout toggles, is session-only until GSettings lands. Flipping it off swaps the stack's transition type to `None` and makes the body-render path skip its fade dance (no routing through the empty page, no waiting for the WebView to paint), restoring the instant pre-fade behavior.
- **UI**: the reading pane's crossfade is race-condition-proofed. The old per-render `load-changed` handler attached a fresh one-shot per HTML body render, so rapid selection changes could accumulate handlers that double-revealed or revealed a stale email after the user had already moved on; the reveal now goes through a single persistent handler gated by a `pending_html_reveal` flag in `UiState`, which the selection handler disarms on every selection change so an in-flight load for an abandoned message can never reveal. The text-body path also now waits out the full fade-out duration instead of a hardcoded 200ms (it reads the stack's real `transition_duration`) and defers its reveal through the same `reveal_message_page` helper, which re-checks `is_transition_running` so the next message can't pop in mid-fade on fast clicks.
- **UI**: the reading-pane header no longer jumps to the next email mid-fade. It was updated synchronously in the selection handler - before the previous message's fade-out even started - so during the 100ms crossfade you'd see the *next* message's subject/sender over the still-fading *previous* body. The header update is now deferred: the selection handler stashes the summary in a `pending_header` in `UiState`, and `render_body` applies it only when the new body actually renders (the pane is on the "empty" placeholder by then, so the swap is invisible until the message page fades back in).

## 0.6.17 (2026-08-04)

### Changed

- **Build**: `build.sh` now defaults to an optimized release build (`cargo build --workspace --release`, binary at `target/release/lookout`) instead of the unoptimized debug build. `--debug` opts back into the fast unoptimized build for day-to-day iteration, and the old `--release` flag still works unchanged.

### Fixed

- **Mail**: Delete/Archive/Report now recognize the Trash/Archive/Junk folders on providers that don't use the default names. The role heuristic (`guess_role_from_name`) only knew "Trash"/"Deleted Items"/"Deleted Messages" for Trash, so a provider whose trash is named "Bin" (Gmail en-GB, `[Gmail]/Bin`), "Recycle Bin", or just "Deleted" got role `Custom` and Delete failed with "No Trash folder found". Added those spellings (plus "Deleted" and "Recycle Bin") and covered each provider spelling with a regression test.
- **Mail**: deleting a message no longer drops the selection in the message list. The list is rebuilt from scratch on every `MessagesUpdated` event, and `remove_all` was resetting the `SingleSelection` to "nothing selected" - so after deleting (or archiving/snoozing) via the hover/toolbar button, the highlight was lost. The handler now snapshots the selected message (mailbox + UID) before the rebuild and restores it afterward: the same message if it's still present, otherwise the message that now occupies the deleted row's old position.

## 0.6.16 (2026-08-04)

### Added

- **UI**: the menu bar's Home/View buttons are now real ribbon tabs instead of disabled placeholders. Home and View are mutually-exclusive toggle tabs (the same `set_group` trick as the nav rail) that switch the ribbon content row below: Home shows the existing command toolbar (New/Reply/Reply All/Forward/Delete/Archive/Report/Snooze), and View is a new "Layout" group with three pane-visibility toggles - Folder pane, Reading pane, and the Mail screen's Calendar-overview pane - each hiding/showing its pane live via `set_visible` on the `Gtk.Paned` child. The tabs are Mail-only: they grey out while a Calendar or Config module is active (those keep their own non-tabbed command toolbars), and returning to Mail restores the last-active tab. The active tab is styled with a pressed-state background, and the overview-pane toggle is respected on module round-trips (hiding it in View stays hidden after visiting Calendar/Config). The layout toggles are session-only until Phase 5's GSettings landing.
- **UI**: the folder view now defaults to the first account's Inbox on startup - the first `FoldersUpdated` event auto-selects the first account's Inbox in the folder tree (accounts sort by label, folders Inbox-first), routing through the same `selected-item` handler as a real click, which sets the current mailbox and issues the `SyncMailbox` that fills the message list. Guarded by `current_mailbox` being unset, so later folder resyncs never yank the user's selection away.

## 0.6.14 (2026-08-03)

### Changed

- **Calendar**: the Day/Week/Work week time grids are now a real timeline instead of fixed per-hour buckets. Each grid is a split view: a fixed all-day band (drawn on its own canvas, never scrolls away) above a vertically scrollable 24-hour day (48px per hour slot, 8am-6pm shaded as business hours, hour markers down the left gutter). Event chips are positioned by their exact start/end time rather than dropped into a per-hour cell - so short and long events render to scale, overlapping events split their day column into side-by-side lanes, and multi-day events stretch across day columns as a single full-width chip. Switching to a time-grid view auto-scrolls it to the current time, and hovering a chip raises a tooltip with the event’s summary and time range plus a white highlight ring. The agenda lists are now grouped under “Today”/“Tomorrow”/date headers.

## 0.6.13 (2026-08-03)

### Fixed

- **Calendar**: the mini calendar now only bolds the current day and days that actually have events, while the sidebar’s “My calendars” list keeps account names bold and calendar row labels at normal weight.

## 0.6.12 (2026-08-03)

### Changed

- **Calendar**: the "My calendars" checklist's per-calendar checkboxes are now custom multi-select radio rows that finally render each calendar's assigned colour. The stock GTK/libadwaita `CheckButton` paints its `.check` indicator through an internal widget whose fill is drawn in a way that ignores display-level CSS overrides (`background-color`, `background-image` and `-gtk-icon-source` at `STYLE_PROVIDER_PRIORITY_APPLICATION` all left it stuck on the theme's accent colour), so the colour could never win. Each row is now a flat `ToggleButton` carrying a hand-drawn 16px radio indicator (a `DrawingArea` painted with Cairo): checked calendars show a solid disc in the calendar's colour with a white inner dot, unchecked ones a hollow ring in that colour - the same colour the calendar's event chips use. Row backgrounds are fully translucent in every state (base/hover/active/checked), so the sidebar background shows straight through the lines. Behaviour is unchanged - multiple calendars stay selectable, toggling still filters the grids, and newly discovered calendars still default to checked.

## 0.6.11 (2026-08-03)

### Added

- **Calendar**: the command toolbar's view-switcher is now fully live instead of a Month-only placeholder. Day, Work week, Week, Month and Split view are real, mutually-exclusive segmented options wired to the main panel's new view stack (anchored to a single shared date so every view agrees on what's displayed, and the header's Today/prev/next navigate in the active view's natural unit - days for Day, weeks for Week/Work week, months for Month/Split). Day and Week/Work week are read-only time grids (all-day row plus one cell per hour against an hour-ruler gutter); Month is the existing Sunday-first grid; Split pairs the month grid with a day-agenda list. An agenda-style list view (chronological, from the anchor date to its month's end) is also available as the Split view's right-hand pane, mirroring the Mail-screen overview pane's day list. All views remain read-only - event creation is still a separate roadmap item.

## 0.6.10 (2026-08-03)

### Fixed

- **Calendar**: Google (and most other CalDAV servers) pretty-print their `multistatus` XML - properties and their `<href>` values sit on their own indented lines. The WebDAV response parser was accumulating that formatting whitespace into property text, which corrupted the `calendar-home-set` href (trailing `\n    ` made the calendar-list PROPFIND hit a non-existent URL) and replaced `resourcetype`'s `collection,calendar` markers with a whitespace blob (so every calendar failed the calendar-type filter). Net result: connected Google accounts silently reported zero calendars and the "My calendars" checklist stayed empty - the exact behavior Evolution sidesteps via its own e-d-s parsing. Fixed by trimming final property values and treating whitespace-only text as absent for container properties; regression-tested with a Google-style pretty-printed response.

## 0.6.9 (2026-08-03)

### Fixed

- **Calendar**: the sidebar's "My calendars" checklist no longer stays blank while accounts are connected but haven't reported their calendars yet. It now renders one entry per connected calendar account (a dim account header), each with its discovered calendars as checkboxes - or, until they arrive, an inline status line: "Connecting…" while the CalDAV session is establishing, "No calendars found" if the account comes up with zero collections, or the session's error message when discovery fails. A "No calendars connected" placeholder shows when there are no calendar accounts at all, so the section always reflects reality instead of silently disappearing.

## 0.6.8 (2026-08-03)

### Added

- **UI**: the Config view's "Advanced" section is now live (rather than a disabled placeholder) with a "Clear all caches" action that deletes the on-disk mail SQLite caches (`$XDG_CACHE_HOME/lookout/mail/`), drops the in-memory calendar occurrences, and asks every connected Mail/Calendar account to resync so the caches rebuild from the servers immediately instead of on next launch.

## 0.6.7 (2026-08-03)

### Added

- **UI**: a new Config view on the left nav rail (third toggle button, `preferences-system-symbolic`, grouped with Mail/Calendar) showing a settings screen (`config_view.rs`): a live account overview - one row per connected Mail account (display name, email, IMAP/SMTP `host:port` from the account's stored connection config) and per Calendar account (display name, CalDAV base URL) - plus disabled placeholder sections mirroring the Phase 5 settings taxonomy (General/Appearance/Layout/Mail/Privacy/Apps/Advanced). Rows repopulate on every switch to the view and again whenever account discovery lands. An "Add account…" row (and matching command-toolbar button) opens GNOME Online Accounts settings, same invocation as the empty-state page's button.
- **UI**: the Config nav-rail button is anchored to the bottom of the rail (below a `vexpand(true)` spacer), matching the Mail/Calendar buttons at the top - a standard app-shell "settings at the bottom" placement.

## 0.6.6 (2026-08-02)

### Changed

- **UI**: the message-row hover quick actions (Archive/Delete/Reply) are now densely packed (zero spacing between buttons) and sit on a solid, opaque pill background (new `.hover-quick-actions`/`.hover-quick-action` CSS classes) overlaid on top of the message line, so they no longer blend transparently into the row content.
- **Build**: `build.sh` now builds everything in the repo - the Rust Cargo workspace and the Next.js webmail frontend - and reports the resulting binary path (`target/debug/lookout` or `target/release/lookout`) at the end of the build.

## 0.6.5 (2026-08-02)

### Changed

- **UI**: the message list now shows each message's date only, formatted according to the system's regional settings (GLib's locale-aware `%x`). Emails received within the last 24 hours instead show the time (`%X`) rather than the date, so fresh messages are easier to spot at a glance.

## 0.6.4 (2026-08-01)

### Fixed

- **Build**: resolved the app build breakage in the window UI by updating the message-row implementation to match the current GTK 4 APIs and mailbox identifier layout, restoring a successful local build and launch.

### Added
 - **Hover Quick Actions**: for message rows are now working as well.
 - **Calendar Sidebar**: the "My calendars" list now repopulates from GOA-discovered CalDAV collections as accounts connect, and newly discovered calendars are shown by default in the month view.

## 0.6.3 (2026-07-30)

### Added

- **UI**: the reading pane now has its own header above the message body (new `message_header` module) - subject line, a colored initials avatar, sender name/email, a "To:" recipient line, the message date, and a second set of Reply/Reply-All/Forward buttons (duplicating the top command toolbar's by design). Populated the instant a message is selected from its `EmailSummary` rather than waiting on the async body fetch, and hidden automatically (via the reading pane's existing stack-page signal) whenever the empty placeholder or the in-place composer is showing instead.

## 0.6.2 (2026-07-30)

### Changed

- **UI**: New Message, Reply, Reply All, and Forward now open the composer in place of the reading pane's content instead of as a separate modal window - a new `"compose"` page in the reading pane's existing `gtk::Stack`, matching how it already flips between the HTML/plain-text/empty views. Cancel or Send returns to whatever was showing before (the same message for Reply/Forward, the empty placeholder for New Message). Selecting a different message while composing silently abandons it, consistent with the app's existing no-confirmation-dialog convention.

## 0.6.1 (2026-07-30)

### Added

- **Mail**: Reply, Reply All, and Forward toolbar buttons now open the compose window pre-filled from the currently selected/open message - `Re: `/`Fwd: ` subject (not doubled up if already present), quoted original body ("On {date}, {sender} wrote:" with `> `-prefixed lines for Reply/Reply-All; a "Forwarded message" header block with the original body verbatim for Forward), and correct `In-Reply-To`/`References` threading headers for Reply/Reply-All (Forward starts a new, unthreaded conversation). Reply-All also carries over the original's other To/Cc recipients, excluding the replying account's own address.
- **Mail**: the compose window gained a Cc field (previously To/Subject/body only), needed for Reply-All to actually carry Cc recipients through to the sent message.

### Fixed

- **Mail**: fixed a would-be double-bracketing bug in the new reply-threading code before it ever shipped - `mail-builder`'s outgoing `Message-Id`/`References`/`In-Reply-To` headers add their own `<>` wrapping, so raw header values pulled from an original message (which already have brackets) are now stripped of them first.

## 0.6.0 (2026-07-30)

### Added

- **Mail**: Delete, Archive, and Report-as-junk toolbar buttons are now live - each moves the selected message into the account's Trash/Archive/Junk mailbox via IMAP MOVE (RFC 6851) where the server supports it, falling back to COPY + STORE `\Deleted` + EXPUNGE otherwise. If an account has no mailbox for that role, the move fails with a toast rather than silently permanent-deleting.
- **Mail**: Snooze is now live - hides the selected message from the list until tomorrow at 9:00 AM local time. Purely client-side (IMAP has no native snooze concept): tracked in a new `snoozed` table in the local per-account SQLite cache, filtered out of what's shown to the UI while still being fetched/cached normally. Snooze expiry is only re-checked on the next sync (IDLE-triggered or explicit), not on a dedicated timer.
- All four actions surface an `AdwToast` on completion ("Deleted"/"Archived"/"Reported as junk"/"Snoozed until tomorrow 9:00 AM") and trigger an immediate folder + message-list resync, matching the existing toast-only feedback convention.

## 0.5.3 (2026-07-30)

### Added

- **UI**: the Mail screen now has a calendar overview pane docked to the far right of the window, spanning the same full height as the nav rail (a sibling in `window_body`, not nested inside `root_stack`) - a mini month-picker plus a day-agenda list below it. Clicking a date shows that day's events (filtered to checked calendars) and triggers a background `CalendarCommand::SyncMonth` for that month without disturbing the main Calendar view's own displayed month. Today's events populate automatically shortly after a calendar account connects, via each account's existing auto-sync-current-month behavior. Hidden while the Calendar view is active, since that view already has its own sidebar mini-calendar. Resizable via a new `content_and_overview_paned` split, matching every other pane boundary in the app.

### Fixed

- **UI**: the overview pane's mini-calendar was rendering roughly 2x wider than intended and had no resize handle - it was appended directly into a plain `gtk::Box` with nothing constraining its natural width (unlike the Calendar view's own sidebar, which caps width via `build_sidebar()`'s `width_request`). Added a matching `width_request(240)` and moved the pane into the new resizable `content_and_overview_paned` split.

## 0.5.2 (2026-07-30)

### Changed

- **UI**: the main (large) calendar grid now has a dark grey (`#2e2e32`) rounded-corner background (new `.calendar-main-background` class) instead of showing the window background image through it, with margins matching the sidebar's `card_section` gap so both panels sit evenly off the paned divider/window edges.
- **UI**: the nav rail now spans the window's full height *below the title bar* rather than only beside the mail/calendar content - `window_header` stays the one real title bar spanning the full window width at the very top; the menu bar, command toolbar, and content now live in a new `inner_content_box` to the rail's right instead of stacking above it as `AdwToolbarView` top bars.
- **UI**: the menu bar and icon command toolbar are now visually grouped - a shared black `.window-toolbars-background` behind both rows, with the icon toolbar additionally boxed into its own `#2e2e32` dark grey, rounded-corner (`.window-icon-toolbar-background`), 6px-margined subgroup distinct from the menu bar above it.

## 0.5.1 (2026-07-30)

### Added

- **Calendar**: reworked the calendar view to match an Outlook-style month layout. The main grid is now Sunday-first with day numbers top-left in a flat hairline grid (new `.calendar-day-cell`/`.calendar-today-cell` CSS classes) instead of libadwaita's rounded `.card` panels; today gets a bordered highlight. A new resizable sidebar (mirroring the Mail folder pane's split) holds a mini month-picker (`MiniCalendar`, clicking a day jumps the main grid to that month) and a live "My calendars" checklist - unchecking a calendar actually filters its events out of the grid, via new `refresh_calendar_checklist`/`refresh_displayed_calendar_view` helpers. A Calendar-specific command toolbar (New event, Day/Work week/Week/Month/Split-view segmented control, Filter/Share/Print - all disabled placeholders except the working Month view) now swaps in for the Mail toolbar via a new `view_toolbar_stack` when the nav rail's Calendar button is active.

## 0.5.0 (2026-07-30)

### Added

- **Calendar**: Phase 3 basics - a new `lookout-dav` crate is a CalDAV client mirroring `lookout-mail`'s architecture: RFC 4791 discovery (principal → calendar-home-set → calendar list), a `calendar-query` REPORT event fetch over `reqwest`, iCalendar parsing (`icalendar` crate, with `TZID`→UTC resolution via `chrono-tz`), and window-bounded RRULE expansion (`rrule` crate). A `CalendarSession` actor mirrors `AccountSession`'s backoff/command-loop shape, polling every 5 minutes in place of IMAP IDLE (CalDAV has no long-poll equivalent).
- **Calendar**: `lookout-goa` now discovers GOA `Calendar`-interface accounts (`list_calendar_accounts`) alongside Mail, as a fully independent account set - a Calendar-only account (no Mail interface) is now correctly picked up, confirmed via an extended fake-GOA D-Bus test.
- **Calendar**: `lookout-core` gains `CalendarId`/`EventUid` ids and `CalendarInfo`/`CalendarEvent`/`EventOccurrence` types, following the existing `Mailbox`/`MailboxId` conventions.
- **UI**: the nav rail's Mail button is now grouped with a new Calendar button, switching to a read-only month-grid view (`calendar_view.rs`) with prev/next/Today navigation. Backed by `spawn_calendar_discovery`/`connect_calendar_account`, mirroring the existing Mail account-discovery wiring, unioning every connected calendar account's events for the displayed month.
- **Testing**: 20 new `lookout-dav` unit/integration tests, including a full `wiremock`-backed discover→list-calendars→fetch-events flow against a mocked CalDAV server.

### Known limitations (tracked for follow-up in `TODO.md`)

- Read-only month view only - no week/day/agenda views, event create/edit/drag, iMIP invitation banners, `.ics` import/webcal, VTODO tasks, birthdays, or notifications yet.
- The GOA `calendar-password` credential-slot id and whether Google's `Calendar.Uri` needs special-casing beyond the generic discovery flow are both unverified against a real live account.

### Fixed

- **Calendar**: a `RefCell` double-borrow panic in `calendar_view::build()` that crashed the app on startup - the initial `set_month` call was passed a still-borrowed value from the same `RefCell` it then tried to `borrow_mut()`.

## 0.4.0 (2026-07-29)

### Added

- **UI**: an Outlook-style menu bar (File / Home / View / Help) and command toolbar (New mail, Delete, Archive, Report, Flag/Unflag, Snooze, More) now span the full window width above the nav rail and mail panes, stacked below the title bar via `AdwToolbarView::add_top_bar`. File → Quit and Help → About are wired to real `gio::SimpleAction`s (`app.quit`, `app.about`, the latter presenting a real `AdwAboutDialog`); everything else without a backing implementation yet (Home, View, Delete, Archive, Report, Flag/Unflag, Snooze, More) is a real but disabled button rather than a dead click. `compose_button` ("New mail") moved out of the header bar into this new toolbar.

## 0.3.7 (2026-07-29)

### Changed

- **UI**: the window background image now renders at full opacity - removed the `set_opacity(0.5)` call on the `gtk::Picture`, which had been carried over from earlier tuning.
- **UI**: the view-switcher rail now has a 6px left margin, matching the gap `card_section`'s default margin already puts between the rail and the folder pane, so the spacing reads as even on both sides of the rail.

## 0.3.6 (2026-07-29)

### Changed

- **UI**: the view-switcher rail's "Mail" button now shows Lookout's own full-color app icon (`data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg`, embedded the same `include_bytes!` way as the background image) instead of the flat `mail-unread-symbolic` icon.

## 0.3.5 (2026-07-29)

### Added

- **UI**: a narrow view-switcher rail now runs along the window's very left edge, outside the folder pane, holding a single active "Mail" icon button for now. Deliberately unstyled (no `.card`, no background), so the window background image shows straight through it. Sits outside `root_stack` so it stays visible on both the "no accounts" empty state and the normal mail view. Only one view exists today, but the button is grouped so more views (Calendar/Contacts/etc., mirroring the reference webmail app's `NavigationRail`) can be appended later as mutually-exclusive toggle buttons.

## 0.3.4 (2026-07-29)

### Added

- **UI**: folder-pane tree rows now show a role-appropriate icon to the left of each folder's label (Inbox/Sent/Drafts/Trash/Junk/Archive each get a distinct icon, other folders get a generic folder icon), mirroring the reference webmail app's per-role sidebar icons. Driven by the `MailboxRole` each folder already carries - no new data plumbing needed, just a `role` -> icon-name mapping in `window.rs`. Account group header rows stay icon-less.

## 0.3.3 (2026-07-28)

### Changed

- **UI**: swapped the embedded window background image from `backgropund1.jpg` to `background2.png` (`Assets/backgrounds/`), still compiled into the binary via `include_bytes!` and drawn the same way (reduced opacity, behind the mail panes).

## 0.3.2 (2026-07-28)

### Changed

- **UI**: the folder pane and message-list pane now sit flush against each other - no gap, no rounded corners on the touching edge (new `.card-flush-end`/`.card-flush-start` CSS classes), and the `Gtk.Paned` handle between them shrinks from 12px to 6px with no painted background (scoped via a new `.seamless-paned` class so the message/reading split keeps its usual handle). Hovering the boundary still shows a resize cursor and drags as before; only the visual gap and handle went away.

## 0.3.1 (2026-07-28)

### Changed

- **UI**: the reading pane's empty state ("No Message Selected") no longer shows an icon or title - it's now a bare placeholder that goes fully transparent (zero alpha, via a new `.reading-pane-transparent` CSS class toggled off the reading stack's `visible-child-name`) so the window background image shows straight through with no card tint. The pane reverts to its normal opaque card as soon as a message is displayed.

## 0.3.0 (2026-07-28)

### Added

- **UI**: the main window now renders a background image (`Assets/backgrounds/backgropund1.jpg`) behind the mail panes, embedded into the binary at compile time and drawn at reduced opacity so it doesn't compete with the foreground UI.

### Changed

- **UI**: the folder pane's translucent background is now a hardcoded black at 50% alpha instead of tracking the current theme's card color, so it reads consistently against the new background image in both light and dark mode.

## 0.2.3 (2026-07-28)

### Added

- **Accounts**: full multi-account support - every GOA account with Mail enabled now connects and syncs concurrently, not just the first one discovered. The folder pane shows one collapsible group per account (auto-expanded on first load), each expanding into that account's own folder tree. Confirmed live against 3 real Gmail accounts connecting and syncing their inboxes simultaneously.
- **Accounts**: compose's "From" address now defaults to whichever account's folder or message is currently selected, instead of being hardcoded to a single connected account.

### Changed

- **UI**: the reading pane now has a 300px minimum-height floor so it can't be squeezed down to something unusably short if the window itself is resized very short.
- **Accounts**: `lookout-goa`'s `GoaClient` is now `Clone` (cheap - `zbus::Connection` is itself Arc-backed), so one D-Bus connection is shared across all accounts' credential providers instead of opening a redundant one per account.

### Fixed

- **Accounts**: a background IDLE resync on one account could no longer clobber the message list while viewing a different account's folder - only `MessagesUpdated` events matching the currently-selected mailbox are applied to the list.

## 0.2.2 (2026-07-28)

### Changed

- **UI**: replaced the nested `AdwNavigationSplitView`s with nested `Gtk.Paned`, giving the three mail panes (Folders / Messages / Reading) a real draggable resize handle. Each pane now renders as its own rounded `.card` panel with a visible gap between them, header-less (the per-card `AdwHeaderBar`s were dropped entirely - titles weren't load-bearing and removing them let the cards read more cleanly). The compose button moved from the Messages pane's header to the single top-level window header bar.
- **UI**: default window size increased from 1100x720 to 1600x900.

### Fixed

- **UI**: every per-card header bar was independently showing its own minimize/maximize/close buttons (four sets of window controls in one window) - `AdwNavigationSplitView`/`AdwNavigationPage` used to coordinate which single header owned the window chrome, and that coordination was lost when they were replaced. Fixed by giving the window exactly one real title bar and disabling title buttons everywhere else. Moot now that the per-card headers are gone entirely, but the underlying lesson (something has to own the window chrome once you stop using the Navigation widgets) stays relevant for future layout work.
- **UI**: removed `Gtk.Paned`'s default painted grey separator line via a scoped CSS provider - the card margins already provide the visual gap between panes, so the painted handle on top of that just looked like a stray line. The handle keeps a comfortable draggable hit-area; only its background/border painting was stripped.

## 0.2.1 (2026-07-28)

### Fixed

- **Mail**: the HTML reading pane always rendered blank white, regardless of content. The `decide-policy` handler installed to block link-clicks from navigating the viewer was blocking *every* `NavigationAction` decision, including the WebView's own initial `load_html()` load - so no message body ever actually rendered, silently. Now distinguished via `NavigationAction::is_user_gesture()`: real clicks are still blocked, the programmatic initial load isn't. Confirmed fixed against a real HTML email (a Google Calendar invite, full styling and all).

## 0.2.0 (2026-07-28)

### Added

- **Mail**: Compose window (plain-text body) with Send, wired through to a real SMTP submission (`lettre`, XOAUTH2/password auth) followed by an `APPEND` of the sent message into the account's Sent mailbox. Validated live end-to-end against Gmail, including confirming the archived copy in Sent.
- **Mail**: Local SQLite cache of mailbox and message metadata per account (`$XDG_CACHE_HOME/lookout/mail/`), used for a fast first paint on startup before the live IMAP fetch completes.
- **Mail**: `Gio.NetworkMonitor`-driven reconnect - a disconnected session's exponential backoff wait is cut short as soon as connectivity is reported back, instead of always waiting out the current delay.
- **Packaging**: `.desktop` entry and AppStream metainfo (both pass their respective validators), a placeholder app icon, and a Flatpak manifest spike with the GOA session-bus permission (`--talk-name=org.gnome.OnlineAccounts`) confirmed against Geary's real, shipping manifest rather than guessed. Not yet a working Flatpak build - see `flatpak/README.md` for exactly what's left (vendored cargo sources, `flatpak-builder` itself, and a sandboxed-runtime fix for the "Open Online Accounts Settings" button).
- **Testing**: a fake-GOA D-Bus test service (`crates/goa/tests/fake_goa.rs`) giving real D-Bus-wire coverage of account discovery and credential fetching without a live GNOME session - run and passing. A GreenMail-backed IMAP/SMTP integration test (`crates/mail/tests/imap_integration.rs`) exercising the real session actor end-to-end - written and compiling clean, but not yet run (no Docker in the environment that wrote it); `#[ignore]`d until someone with Docker runs it for real.

### Known limitations (tracked for follow-up)

- Compose is plain-text only (no rich-text/contenteditable body, no recipient chips) and single-identity (always sends as the account's own address).

## 0.1.0 (2026-07-27)

Initial scaffolding of Lookout, a native GNOME/libadwaita mail client written in Rust, reimplementing the UI and functionality of [Bulwark Webmail](webmail) against IMAP/SMTP via GNOME Online Accounts instead of JMAP.

### Added

- **Accounts**: GNOME Online Accounts discovery over D-Bus (`lookout-goa`) - lists Mail-enabled accounts, filters out accounts without usable IMAP/SMTP support, and fetches OAuth2 access tokens or passwords on demand without caching credentials locally.
- **Mail**: IMAP account-session actor (`lookout-mail`) with XOAUTH2 and password login, `LIST (SPECIAL-USE)` folder discovery with role detection (Inbox/Sent/Drafts/Trash/Junk/Archive), envelope fetch with RFC 2047 subject/name decoding, and full-message body fetch/parsing via `mail-parser`.
- **Mail**: IMAP IDLE-based live sync, with on-demand commands (open a folder, fetch a message body) interrupting IDLE immediately rather than waiting on a timeout.
- **Mail**: Client-side conversation threading (JWZ-style, adapted for IMAP's lack of a native thread id) in `lookout-core`.
- **UI**: GTK4/libadwaita application shell (`lookout-app`) with a nested `AdwNavigationSplitView` layout (folders / messages / reading pane), an empty-state page that deep-links to `gnome-control-center online-accounts` when no mail account is configured, a folder tree, a message list, and a reading pane that renders HTML mail in a sandboxed WebKit view (JavaScript disabled, navigation blocked) with a plain-text fallback.
