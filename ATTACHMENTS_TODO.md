# Attachment preview (M-32, X-03; roadmap 7.3)

This plan adds **in-app attachment preview** to the Mail module: images, PDFs and text/code
attachments open in a Lookout preview window instead of requiring an external application, and
**Preview** becomes the default click on an attachment tile. It also adds Outlook's attachment
tile menu shape (**Preview / Open / Open With… / Save…**) and the file-type icon and size display
that go with it.

The work closes the last real Mail gap in `OUTLOOK_PARITY.md` §5.1 (**M-32**) and the matching
Desktop row (§5.8 **X-03**). It is roadmap item **7.3**.

**Status:** planned; nothing is implemented yet.

**Effort:** `M` for images + PDF + text (about one week). `L` if the optional LibreOffice
conversion for Office documents is included.

---

## Scope

### In scope

- An in-app preview window for:
  - images (`image/*`, including SVG where the system loader supports it);
  - PDF (`application/pdf`, `application/x-pdf`);
  - text and source files (`text/*`, JSON, XML, CSV, and extension-based fallbacks such as
    `.txt`, `.md`, `.log`, `.csv`, `.json`, `.xml`, `.html` shown as **source**, never
    rendered);
  - Office documents through an optional LibreOffice-to-PDF conversion (`docx`, `xlsx`,
    `pptx`, ODF, legacy Office, RTF).
- **Preview as the default click** on a reading-pane attachment row and a conversation-card
  attachment tile.
- The tile menu: **Preview, Open, Open With…, Save…** (keeping Lookout's existing Save verb;
  Outlook calls this Download / Save to).
- File-type icon, name and human-readable size on each attachment tile.
- Open / Open With… / Save… from inside the preview window, reusing the existing flows.
- Loading, unsupported, too-large, password-protected and fetch-failed states.
- Keyboard access: Enter on a focused tile opens Preview; Page Up/Page Down and the page
  controls navigate a PDF.

### Out of scope

- Compose attachments. The composer already has its own attachment strip; previewing there is a
  separate UI change.
- Calendar event attachments (`CAL-21`). Those already have Save/Open in the event editor and
  invitation card.
- Inline `cid:` images. They already render in the message body and are not attachments.
- `.eml` / `.msg` / `.vcf` / `.ics` previews. `.eml`/`.msg` are separate roadmap items
  (`1.22`, `7.2`); an `.ics` attachment can be shown as text but is not parsed into an event
  preview.
- Text selection or search inside a PDF. The first cut renders pages; it does not build a text
  layer.
- Printing from the preview window. Use Open for a full application.
- A server-side or cross-device preview cache. Fetched bytes already land in the existing
  attachment flat-file cache; preview state itself is session-only.
- Any new OAuth scope, cache table or `user_version` bump. Preview consumes the existing
  `AccountEvent::PartFetched` path on both IMAP and Graph mail.

---

## Current state

### Reading pane and message window

- `source/crates/app/src/window/attachments.rs`:
  - `rebuild_attachment_strip` (`:46`) clears the named `"attachments"` box and builds one
    horizontal row per `BodyPart` where `is_attachment` is true;
  - each row is a name + size label plus a `gtk::MenuButton` with **Open**, **Open With…** and
    **Save…**;
  - double-clicking a row is the same as Open;
  - the strip header offers **Save all** and **Download as zip…** for two or more attachments;
  - `start_attachment_fetch` (`:219`) records one in-flight action in
    `UiState::pending_attachment` and sends `AccountCommand::FetchAttachment`;
  - `save_attachment_to_disk` (`:593`) and `open_attachment_temp` (`:752`) take an
    `adw::ApplicationWindow`, which will need relaxing for a preview window;
  - `attachment_extension` (`:665`) already maps common MIME types and fallbacks to a safe file
    extension, and `attachment_display_name` (`:27`) already sanitizes filenames.
- `source/crates/app/src/message_window.rs` (`:625`) reuses
  `rebuild_attachment_strip` verbatim, so a strip change reaches the popped-out message window
  automatically.
- `source/crates/app/src/window/reading.rs`:
  - `find_attachment_strip` (`:333`) locates the strip inside the reading pane's `"message"`
    page;
  - `message_is_visible` (`:1032`) decides whether a late `PartFetched` belongs to the message
    on screen. A preview fetch must **not** depend on this: the preview window stays open while
    the user navigates the reading pane.

### Conversation cards

- `source/crates/app/src/conversation_view.rs`:
  - `AttachmentTile { part_number, name, size_label }` (`:67`);
  - `card_extras_html` (`:380`) renders one `<button class="tile" data-action="attachment">`
    per attachment;
  - `parse_script_message` (`:145`) maps the script's `attachment` action to
    `ScriptMessage::Attachment`.
- `source/crates/app/src/window/conversation.rs`:
  - `ScriptMessage::Attachment` (`:672`) looks up the `BodyPart` and calls
    `present_conversation_attachment_menu` (`:740`), the Open / Open With… / Save… / Save all
    / Zip popover.
- There is no separate menu affordance on a card tile; a click always opens that menu.

### Session and cache

- `AccountCommand::FetchAttachment` and `AccountEvent::PartFetched` already exist in
  `source/crates/mail/src/session.rs` (`:377`, `:846`).
- Both transports answer with **transfer-decoded content bytes**:
  - IMAP: `fetch_attachment_part` calls `lookout_mail::body::transfer_part_bytes` (`:7726`);
  - Graph: `source/crates/graph/src/mail/session.rs` (`:1113`) returns the same
    `PartFetched` shape.
- TNEF's synthetic `tnef:<source>:<n>` parts therefore work without any transport change.
- `lookout_mail::body::decode_text_part` (`:893`) decodes *wire* bytes and must **not** be used
  directly on `PartFetched` bytes: it would apply `Content-Transfer-Encoding` a second time.
- Attachment bytes are already cached on disk by the session, so reopening a preview does not
  normally re-fetch from the server.

### UI state

- `source/crates/app/src/window/state.rs`:
  - `PendingAttachmentAction` (`:894`) is Open / OpenWith / Save;
  - `PendingAttachment` (`:911`) is the one-at-a-time strip action;
  - `UiState::pending_attachment` and `pending_attachment_batch` (`:1400`, `:1404`);
  - `UiState::message_details` (`:1439`) is the precedent for a single dialog with its own
    late-response routing;
  - `UiState::temp_attachment_files` (`:1526`) is cleaned at shutdown.
- `source/crates/app/src/window/connect.rs` handles `PartFetched` (`:936`) and
  `PartFetchFailed` (`:1009`) in one event loop. A preview branch will be added there.
- `source/crates/app/src/window.rs` initialises `UiState` (`:652`), and
  `source/crates/app/src/window/tests.rs` has a `UiState` literal that must gain any new field.

### Styling and icons

- `source/data/styles/message-list.css` holds `.attachment-row` and its hover state.
- `source/crates/app/src/style.rs` concatenates the static stylesheet files; a new
  `attachment-preview.css` can be added to that list.
- The app has both GNOME and Fluent icon packs (`icon_style.rs`), so use symbolic names that
  exist in both, with `mail-attachment-symbolic` as the final fallback.

---

## Design

### Preview surface

Use a single reusable, non-modal **`adw::Window`** per Lookout window, modelled on
`message_window` and `message_details`:

- title = the attachment's display name, subtitle = type and size;
- a slim action bar with **Open**, **Open With…**, **Save…** on the left;
- PDF page controls (Previous, page N of M, Next, zoom/fit) on the right;
- a non-homogeneous `gtk::Stack` with pages:
  - `loading` — spinner and “Loading attachment…”;
  - `image` — `gtk::ScrolledWindow` + `gtk::Picture` (`ContentFit::Contain`,
    `can_shrink(true)`);
  - `pdf` — `gtk::ScrolledWindow` + `gtk::Picture`, one rendered page at a time;
  - `text` — `gtk::ScrolledWindow` + read-only monospace `gtk::TextView`
    (`WrapMode::WordChar`), capped at about 512 KB / 10,000 lines with a “Preview truncated”
    affordance;
  - `unsupported` — `adw::StatusPage`, “No preview available”, with a note to use Open or Save;
  - `toolarge` — `adw::StatusPage`, “This file is too large to preview”;
  - `error` — `adw::StatusPage`, fetch/render failure plus Retry.
- Re-opening the same attachment re-presents the existing window. Opening a different
  attachment replaces its contents (keep the window size and position), clears the old page and
  starts a new fetch.
- Closing the window cancels its pending preview fetch and invalidates any in-flight PDF/Office
  render generation. Late results are ignored.
- Privacy mode: tag the filename label as sensitive and blur the picture/text content, following
  the existing `privacy::mark` / `mark_blur_all` conventions.

### Fetch model

Preview needs its own ledger, separate from the strip's one-at-a-time Open/Save action:

- `UiState::pending_attachment_preview: Option<PendingAttachmentPreview>` with
  `{ mailbox, uid, part, generation }`;
- `UiState::attachment_preview: Option<AttachmentPreviewState>` with the window, the current
  part, its bytes, the content kind and the PDF/Office render state;
- `UiState::preview_generation: u64`, bumped whenever a preview is opened, replaced or closed.

`start_attachment_preview(state, parent, mailbox, uid, part)`:

1. find or create the preview window, present it and show `loading`;
2. bump the generation and record the pending request;
3. if the account is connected, send `AccountCommand::FetchAttachment` on the **interactive**
   channel, as `start_attachment_fetch` does;
4. arm a 60-second timeout; on timeout show the `error` page with Retry.

`PartFetched` handling order in `window/connect.rs`:

1. batch collection (`advance_attachment_batch`);
2. **preview** — match `pending_attachment_preview` by mailbox, uid and part number, take it,
   and call `attachment_preview::show_loaded(...)`; do **not** consult `message_is_visible`;
3. strip action (`pending_attachment`).

`PartFetchFailed` does the same for the preview, showing the error page instead of dropping the
request. A closed preview window clears its ledger first, so a late result is ignored.

### Content classification

Pure, testable `PreviewKind { Image, Pdf, Text, Office, Unsupported }`:

- MIME type first, then a filename-extension fallback (important for
  `application/octet-stream` and Outlook clients that send generic types);
- `text/html` is **Text** (source view), not rendered HTML;
- `application/json`, `application/xml`, `application/javascript` and similar structured text
  are **Text**;
- Office MIME types and extensions map to **Office**;
- `message/rfc822`, archives and executables are **Unsupported**;
- unknown files are **Unsupported**, with Open / Save still available.

Suggested caps:

- preview fetch / render cap: 50 MB (`MAX_PREVIEW_BYTES`);
- text display cap: 512 KB and 10,000 lines;
- PDF page count shown but only the current page rendered;
- image decode cap: reject images whose dimensions exceed a safe bound (use a
  `gdk_pixbuf::PixbufLoader` with `set_size` rather than an unbounded `Texture::from_bytes`).

### Image preview

- Decode the `PartFetched` bytes with `gdk_pixbuf` (bounded dimensions) and wrap the result in a
  `gdk::Texture` for `gtk::Picture`.
- Fit to the window by default; add Fit / Actual size controls only if they fit the first cut.
- A decode failure shows the `unsupported`/`error` page with Open and Save, never a crash or a
  blank window.
- SVG is shown through the system loader; if the loader is missing, it degrades to the
  unsupported page.
- **Do not** load external resources from SVG or HTML attachments.

### Text preview

- Add `pub fn decode_text_attachment(part: &BodyPart, bytes: &[u8]) -> String` to
  `source/crates/mail/src/body.rs`.
- It wraps the bytes with only `Content-Type` and the charset (no
  `Content-Transfer-Encoding`, because `PartFetched` is already decoded) and parses them with
  `mail_parser`, reusing the same charset path as `decode_text_part`.
- If the part has no charset, use UTF-8 with lossy fallback.
- Binary detection: NUL bytes or a high proportion of non-text control bytes switch to
  **Unsupported** (“This file doesn't look like text”).
- HTML attachments show their source, not a live document.
- No syntax highlighting in the first cut; monospace and line wrapping are enough.

### PDF preview

- Add `poppler-rs` (the GLib Poppler bindings) to `lookout-app` with the `v0_82` feature (or the
  lowest feature set that exposes `Document::from_bytes`).
- Keep rendering off the UI thread:
  - `PdfRenderRequest { bytes: Arc<Vec<u8>>, page: usize, scale: f64, generation: u64 }`;
  - `gio::spawn_blocking` creates the Poppler `Document`, renders the page into a Cairo
    `ImageSurface`, writes PNG bytes, and returns `(page_count, png)`;
  - only `Vec<u8>`/`u64` cross the thread boundary; the GTK widgets stay on the main thread.
- `PdfState { page, page_count, zoom, rendered_page, generation }`.
- Controls: Previous, “Page N of M”, Next, Zoom out / Fit / Zoom in.
- Password-protected or malformed PDFs show the error page with Open and Save.
- Render only the current page; do not rasterise a whole large document up front.

### Office preview (optional)

- Detect `libreoffice` or `soffice` with `glib::find_program_in_path`.
- Do **not** convert every Office attachment automatically on click. Show a page with a
  **Preview with LibreOffice** button; conversion runs only when the user asks for it.
- Convert in a private temp directory:
  - write the attachment bytes with the correct extension from `attachment_extension`;
  - run `libreoffice --headless --safe-mode --norestore --nolockcheck --nofirststartwizard
    --convert-to pdf --outdir <dir> <file>`;
  - 30-second timeout, `gio::Subprocess` + `force_exit` on timeout;
  - read the resulting PDF, then feed it to the same `pdf_preview` renderer;
  - remove the temp directory and files on success, failure or window close.
- If LibreOffice is absent or conversion fails, show the unsupported/error page with Open and
  Save. In Flatpak/snap the host LibreOffice may not be visible; that is a normal degraded state.
- Office conversion is explicitly **optional** for M-32. The roadmap row already calls it
  optional; images + PDF + text are enough to mark M-32 and X-03 complete.

### Attachment tiles and menus

**Reading-pane strip:**

- Replace each row's plain box with a tile-style row:
  - a file-type symbolic icon (`gio::content_type_get_symbolic_icon`, fallback
    `mail-attachment-symbolic`);
  - a clickable main area (a flat `gtk::Button`) with the name and size;
  - a trailing `gtk::MenuButton` with **Preview / Open / Open With… / Save…**.
- Single click on the main area opens Preview. The menu's Preview item is there for
  discoverability and keyboard use.
- Keep the existing multi-attachment **Save all / Download as zip…** header unchanged.
- Start the preview with the button's root window as parent, so it works from the main window
  and from a popped-out message window.
- Add accessible labels and tooltips: “Preview <name>”, “Attachment actions for <name>”.
- Generalise `open_attachment_temp` and `save_attachment_to_disk` from
  `&adw::ApplicationWindow` to `&impl IsA<gtk::Window>`, matching
  `print_calendar_month`'s pattern. Pass the preview window's own `adw::ToastOverlay` so save
  errors appear where the user is looking.

**Conversation cards:**

- Keep the tile as the main preview button (`data-action="attachment"`), with
  `title="Preview <name>"`.
- Add a small trailing ⋯ button (`data-action="attachment-menu"`) so the existing menu remains
  reachable. The menu gains **Preview** at the top.
- `conversation_view.js` already posts `{ action, card, part, x, y }`; route `attachment` to
  Preview and add `AttachmentMenu` to the script message enum.
- Update the inline CSS in `conversation_view.rs` so the main tile and the ⋯ button share a
  wrapper and the ⋯ button appears on hover/focus.
- **Save all / Download as zip…** on a card stays in the ⋯ menu.

### What this does not need

- No new `AccountCommand` or `AccountEvent`. `FetchAttachment` / `PartFetched` /
  `PartFetchFailed` already carry everything.
- No mail cache migration. Bytes are already stored by the session.
- No Graph scope change. The Graph mail transport answers `PartFetched` the same way IMAP does.
- No GSettings key. Preview is a session-only view; Open/Save/Open With remain the same flows.
- No network access at all beyond the existing account fetch.

---

## Proposed decisions (to confirm before M-32.1)

1. **Preview surface.** A reusable separate `adw::Window` (recommended) versus a page inside the
   reading pane. The separate window works from the reading pane, a popped-out message and a
   conversation card with one implementation, and it does not disturb the selected message.
2. **PDF backend.** Linked `poppler-rs` (recommended; native, no subprocess, adds
   `libpoppler-glib-dev` at build time and `libpoppler-glib8`/`libpoppler-glib8t64` at runtime)
   versus `pdftoppm`/`pdfinfo` subprocesses (no build dependency, needs `poppler-utils` at
   runtime, slower per page). The rest of this plan assumes linked Poppler.
3. **Office preview.** Include LibreOffice conversion in M-32 (optional, adds about a day), or
   ship images/PDF/text first and leave Office as a follow-up. Either is compatible with marking
   M-32/X-03 complete, because the parity row calls Office optional.
4. **Conversation tiles.** Click-to-preview plus a small ⋯ menu (recommended), or keep
   click-to-menu and put Preview first. The recommended shape matches Outlook and keeps Save
   all / zip reachable.

Sizes: `S` under a day, `M` a few days, `L` a week or more.

---

## Task breakdown

### [ ] M-32.1 — Preview classification and tile model (`S`)

Files: new `source/crates/app/src/attachment_preview.rs`; `source/crates/app/src/main.rs`.

- Add `PreviewKind`, `preview_kind(part)`, `preview_title(part)`, `preview_icon_name(part)` and
  the size-cap helpers.
- Classify by MIME type first and filename extension second.
- Treat `text/html` as source text, structured application types as text, Office types and
  extensions as Office, and everything unknown as Unsupported.
- Keep all strings pure (no GTK, no I/O) so the table can be unit-tested.
- Add `mod attachment_preview;` to `main.rs`.

Tests:

- MIME table for image/PDF/text/Office/unknown types;
- extension fallback when the MIME type is `application/octet-stream`;
- uppercase extensions, missing filenames, names containing path separators;
- `text/html` is Text, `message/rfc822` is Unsupported;
- size caps and text truncation boundaries.

### [ ] M-32.2 — Text decoding helper (`S`)

Files: `source/crates/mail/src/body.rs`.

- Add `pub fn decode_text_attachment(part: &BodyPart, bytes: &[u8]) -> String`.
- Do **not** include `Content-Transfer-Encoding` when re-wrapping the bytes; `PartFetched` has
  already undone base64/quoted-printable.
- Use the part's charset; fall back to UTF-8 lossily.
- Add a `looks_binary` helper or return an `Option<String>`/enum so the UI can show the
  unsupported page.
- Keep `decode_text_part` unchanged for the body-fetch path.

Tests:

- UTF-8 and ISO-8859-1 text;
- an already-decoded base64 part is not decoded twice;
- an unknown charset falls back without panicking;
- empty bytes and binary bytes;
- HTML source comes back as source text.

### [ ] M-32.3 — Preview window shell (`M`)

Files: new `source/crates/app/src/attachment_preview.rs`;
`source/crates/app/src/window/state.rs`; `source/crates/app/src/window.rs`;
`source/crates/app/src/window/tests.rs`; `source/crates/app/src/style.rs`;
new `source/data/styles/attachment-preview.css`.

- `AttachmentPreviewState`:
  - the `adw::Window`;
  - current `(mailbox, uid, part_number)`;
  - fetched bytes (dropped on close);
  - `PreviewKind`;
  - the stack, title/subtitle widgets, action bar;
  - PDF/Office render state and generation.
- Build the window with a header bar, a slim action bar and a non-homogeneous `gtk::Stack`.
- Add pages: loading, image, PDF, text, unsupported, too large, error.
- Single-instance reuse: same part re-presents; a different part replaces the content and
  starts a new fetch.
- Close handler: clear `UiState::attachment_preview`, clear the pending preview, bump the
  generation and cancel in-flight conversion.
- Apply privacy classes to the filename and content.
- Register the new CSS file in `style.rs`.
- Initialise the new `UiState` fields in `build_window` and in `window/tests.rs`.

Tests:

- pure title/subtitle/kind selection;
- window state replacement and generation invalidation;
- GTK smoke test for the stack and close cleanup, skipped unless `gtk_test::gtk_ready()`.

### [ ] M-32.4 — Preview fetch plumbing (`S`)

Files: `source/crates/app/src/window/state.rs`;
`source/crates/app/src/window/connect.rs`;
`source/crates/app/src/window/attachments.rs`.

- Add `PendingAttachmentPreview { mailbox, uid, part, generation }`.
- Add `start_attachment_preview(state, parent, mailbox, uid, part)`:
  - create/re-present the window;
  - show loading;
  - record the pending request and bump the generation;
  - send `AccountCommand::FetchAttachment` on the interactive channel;
  - arm the 60-second timeout.
- In `PartFetched`, handle the preview after the batch and before the strip action. Match by
  mailbox, uid and part number; do not call `message_is_visible`.
- In `PartFetchFailed`, show the preview error page when the pending request matches.
- Closing the preview clears the pending request and ignores late results.
- If the account is not connected, show a clear “This account isn't connected” error page.

Tests:

- pure matching function for mailbox/uid/part/generation;
- a stale generation is ignored;
- a closed preview drops the pending request;
- the existing strip action and batch paths still work.

### [ ] M-32.5 — Reading-pane and message-window tiles (`S`)

Files: `source/crates/app/src/window/attachments.rs`;
`source/data/styles/message-list.css` (or `attachment-preview.css`);
`source/crates/app/src/message_window.rs` (verify only).

- Build a tile-style row: file icon, clickable name/size area, trailing ⋯ menu.
- Single click on the main area starts Preview.
- Menu: Preview, Open, Open With…, Save….
- Keep the Save all / zip header.
- Add accessible labels and tooltips.
- Generalise `open_attachment_temp` and `save_attachment_to_disk` to accept any
  `gtk::Window`; use the preview window's toast overlay from inside the preview.
- Confirm the message window's reuse still builds the strip and opens the preview with the
  message window as parent.

Tests:

- `attachment_display_name` and `attachment_extension` stay correct;
- `preview_icon_name` fallback;
- tile label composition and menu labels;
- existing attachment tests continue to pass;
- a GTK test for the tile click, if feasible.

### [ ] M-32.6 — Conversation-card tiles (`S`)

Files: `source/crates/app/src/conversation_view.rs`;
`source/crates/app/src/conversation_view.js`;
`source/crates/app/src/window/conversation.rs`.

- Main tile button: preview action, `title="Preview <name>"`.
- Add a trailing ⋯ menu button with `data-action="attachment-menu"`.
- Add `ScriptMessage::AttachmentMenu`.
- Route `Attachment` to `start_attachment_preview`; route `AttachmentMenu` to the existing
  `present_conversation_attachment_menu`, with **Preview** first.
- Keep **Save all** / **Download as zip…** in that menu.
- Update the inline CSS for the tile wrapper and the ⋯ button.
- Ensure keyboard users can reach the tile and the menu.

Tests:

- `parse_script_message` parses both actions;
- `card_extras_html` contains the preview and menu buttons and escapes the filename;
- the existing conversation-view tests still pass.

### [ ] M-32.7 — PDF preview (`M`)

Files: new `source/crates/app/src/pdf_preview.rs`;
`source/crates/app/Cargo.toml`; packaging files below.

- Add `poppler-rs` with the `v0_82` feature.
- `PdfState { page, page_count, zoom, rendered_page, generation }`.
- Render the current page in `gio::spawn_blocking`:
  - load the `Document` from the bytes;
  - render the page into a Cairo `ImageSurface` at a 2× scale;
  - write PNG bytes;
  - return `(page_count, png)`.
- On the main thread, decode the PNG into a `gdk::Texture` and update the picture and controls.
- Previous/Next/Page Up/Page Down and zoom/fit controls; clamp pages.
- Generation guard: a late render for an old page or a closed window is ignored.
- Password-protected/malformed PDFs show the error page with Open/Save.
- Cap the input size; show the too-large page above `MAX_PREVIEW_BYTES`.

Tests:

- page navigation, clamping and zoom bounds;
- a small checked-in PDF fixture yields a page count and renders a non-empty PNG;
- a malformed PDF returns an error rather than panicking;
- the generation guard drops stale results.

### [ ] M-32.8 — Office preview (`M`, optional)

Files: new `source/crates/app/src/office_preview.rs`;
`source/crates/app/src/attachment_preview.rs`.

- Detect `libreoffice`/`soffice` with `glib::find_program_in_path`.
- Show a **Preview with LibreOffice** action rather than converting automatically.
- Convert to PDF in a private temp directory with a 30-second timeout:
  `--headless --safe-mode --norestore --nolockcheck --nofirststartwizard
  --convert-to pdf --outdir <dir> <file>`.
- Feed the PDF bytes to `pdf_preview`.
- Clean up temp files on success, failure and window close.
- If LibreOffice is missing or conversion fails, show the unsupported/error page with Open and
  Save.
- Sandboxed builds where the host LibreOffice is invisible simply degrade.

Tests:

- command-line construction and extension selection;
- fake converter success/failure/timeout;
- no real LibreOffice invocation in CI.

### [ ] M-32.9 — Close-out (`S`)

Files: `OUTLOOK_PARITY.md`, `SCAFFOLD.md`, `CHANGELOG.md`, packaging/version files, screenshots.

- Update `OUTLOOK_PARITY.md`:
  - §5.1 **M-32** → ✅, Gap “—”, Today text mentions the in-app preview and default click;
  - §5.8 **X-03** → ✅;
  - §2.1 Mail counts **39 / 4 / 1 / 2**; Desktop **5 / 3 / 4 / 0**; total
    **126 / 12 / 14 / 5**; “about 80% at parity, about 8% partial”;
  - §2.2 remove attachment preview from the top ten and renumber; promote **PST import
    (X-02)** into slot 10 (it is next in the doc's own “beyond the top ten” list and is a
    migration blocker), or leave nine items if you prefer not to promote anything;
  - §7.3 → ✅ and struck;
  - **UX-34** updated to describe tiles, file-type icons and Preview as the default click;
  - **P-10 / UX-67** note changed from “the Files tab waits on attachment preview (M-32)” to
    “the Files tab remains a follow-up” (preview alone does not implement the People Files
    tab);
  - refresh the “Refreshed for 0.9.xxx” line.
- Update `SCAFFOLD.md`:
  - add `attachment_preview.rs`, `pdf_preview.rs` and optional `office_preview.rs`;
  - add `pending_attachment_preview` / `attachment_preview` to the `UiState` pipeline;
  - note the Poppler/LibreOffice dependency in the build section.
- Add the next release's `CHANGELOG.md` entry in the existing style, citing **M-32, X-03;
  roadmap 7.3**.
- Bump the workspace version and lockfile, `source/snapcraft.yaml`, the AppStream metainfo and
  the release workflow's tag example.
- Refresh `assets/screenshots/mail_read_light_1.png` and `mail_read_dark_1.png` if the
  attachment strip is visible there (the §6.11 checklist asks for it).
- If Office conversion is deferred, record that in the M-32 note rather than marking it as a
  gap: the parity row explicitly calls Office optional.

---

## Build, packaging and CI changes

These apply only if the linked Poppler option is chosen (recommended).

### Rust and local build

- `source/crates/app/Cargo.toml`: add
  `poppler = { package = "poppler-rs", version = "0.26", features = ["v0_82"] }`
  (confirm the feature level against the oldest supported distro; `v0_82` needs
  poppler-glib ≥ 0.82).
- `source/Cargo.toml` / `source/Cargo.lock`: workspace version and lockfile.
- `build.sh`: add `poppler-glib` to the `pkg-config --exists` check and the Debian/Ubuntu
  install hint.
- `README.md`: add `libpoppler-glib-dev` to the prerequisites command.

### Debian / RPM

- `source/crates/app/Cargo.toml` `[package.metadata.deb].depends`:
  add `libpoppler-glib8 | libpoppler-glib8t64` (verify the exact package name for the
  supported Ubuntu/Debian releases).
- `[package.metadata.generate-rpm.requires]`: add `poppler-glib`.

### Snap

- `source/snapcraft.yaml` build-packages: add `libpoppler-glib-dev`.
- stage-packages: add `libpoppler-glib8t64` (or the core24 equivalent) and `poppler-data`.

### Flatpak

- Verify whether the `org.gnome.Platform` 49 runtime already ships `libpoppler-glib`.
- If not, add a Poppler module to `source/flatpak/io.github.gavindi.Lookout.json`.
- Regenerate `source/flatpak/cargo-sources.json` after adding the Rust dependency.

### CI and release

- `.github/workflows/ci.yml`: add `libpoppler-glib-dev` to the clippy, test and build install
  lists.
- `.github/workflows/release.yml`: add it to the release build install list.

If the `pdftoppm` option is chosen instead, skip the Rust crate and dev-package changes; add
`poppler-utils` as a runtime recommendation and stage it in the snap/Flatpak, and keep the
`PdfRenderer` seam so linked Poppler can replace it later.

---

## Suggested order

1. **M-32.1** classification.
2. **M-32.2** text decoding.
3. **M-32.3** preview window shell + **M-32.4** fetch plumbing (these can land together).
4. **M-32.5** reading-pane and message-window tiles.
5. **M-32.6** conversation cards.
6. **M-32.7** PDF.
7. **M-32.8** Office (optional).
8. **M-32.9** close-out.

Dependencies:

- M-32.3 depends on M-32.1 and M-32.2.
- M-32.4 depends on M-32.3.
- M-32.5 and M-32.6 depend on M-32.4.
- M-32.7 is independent of M-32.5/M-32.6 but must land before close-out if PDF is in the
  required scope.
- M-32.8 depends on M-32.7.
- M-32.9 depends on everything that ships.

---

## Acceptance checklist (§6.11)

- **Placement:** reachable from the reading-pane strip, the popped-out message window and a
  conversation card; Preview is the default click; the menu is Preview / Open / Open With… /
  Save….
- **Wording:** “Preview”, “Open”, “Open With…”, “Save…”, “No preview available”, “This file is
  too large to preview”, “Loading attachment…”.
- **Defaults:** single click previews; Open remains an explicit action; no read/unread change.
- **Shortcut:** Enter on a focused tile previews; Page Up/Page Down navigate PDF pages; no new
  global shortcut (Outlook has no attachment-preview chord).
- **Undo:** not applicable; preview is read-only.
- **Empty/error states:** loading, unsupported, too large, password-protected, fetch failed,
  account disconnected.
- **Accessibility:** every clickable tile and menu button has an accessible name and tooltip;
  the preview controls are keyboard reachable; the text view is read-only.
- **Privacy:** attachment names and content follow the existing blur modes.
- **Screenshot:** refresh the Mail reading-pane screenshots if the strip is visible.

---

## Verification

```bash
cargo fmt --all -- --check
cargo check -p lookout-app --all-targets
cargo clippy -p lookout-app --all-targets -- -D warnings -A clippy::result_large_err
cargo test -p lookout-mail
cargo test -p lookout-app
cargo test --workspace          # after the Poppler dependency is in CI
```

Known test caveat: `lookout-app`'s
`shortcuts::tests::display_accel_is_human_readable` calls GTK from a non-main test thread and can
fail in a headless environment; treat that as pre-existing, as the other release notes do.

Manual checks:

- image, PDF, text, HTML-source and unknown attachments;
- a multi-page PDF (page navigation, zoom, malformed file);
- an Office document with LibreOffice present and absent (if M-32.8 ships);
- a TNEF message with an inner image/PDF attachment;
- a Microsoft 365 account using Graph mail and one using IMAP;
- open from the reading pane, a popped-out message and a conversation card;
- close the preview mid-fetch, then navigate the reading pane;
- privacy mode Off / Sensitive / All;
- keyboard-only operation.

---

## Risks and limitations

- **Poppler build/packaging.** The linked option adds a C dependency to local builds, CI, the
  deb/RPM, snap and Flatpak. The dev machine currently has the runtime library but not
  `libpoppler-glib-dev`, so the first implementation step must install it. The `pdftoppm`
  alternative avoids the build dependency but adds a runtime one and is slower per page.
- **Large files.** Fetching a very large attachment into memory is expensive. Cap preview
  fetching/rendering (50 MB suggested) and show Open/Save instead. Rendering a PDF page must
  stay off the UI thread.
- **Office security.** LibreOffice conversion of untrusted documents is a risk. Keep it
  user-initiated, use a private temp directory, `--safe-mode`, a timeout and cleanup. Do not
  auto-convert on single click.
- **Text double-decoding.** `PartFetched` bytes are already transfer-decoded. Reusing
  `decode_text_part` unchanged would corrupt base64/quoted-printable text; the new helper must
  omit the transfer-encoding header. This is the easiest bug to introduce, so test it
  explicitly.
- **PDF text layer.** The first cut renders images, so users cannot select or search text in a
  PDF. That is acceptable for M-32; note it as a limitation.
- **Message-window parenting.** The preview window should be transient for the window the user
  clicked from, not always the main window. Verify the popped-out message window path.
- **Sandboxed Office preview.** Flatpak/snap may not see the host LibreOffice; the feature
  degrades to Open/Save. Do not try to pierce the sandbox with host spawning.
- **Conversation WebView.** The tile/menu change touches trusted-script markup. Keep all
  message-derived strings escaped and run the existing conversation-view tests.

---

## Open questions

- Confirm the PDF backend before M-32.7: linked `poppler-rs` or `pdftoppm`/`pdfinfo`.
- Confirm whether Office conversion ships in M-32 or as a follow-up. The parity row permits
  either.
- Confirm the preview surface: separate window or an in-reading-pane page. The separate window
  is the recommended and simpler shape.
- Should the reading-pane strip become a true wrapping grid of tiles, or is a tile-style
  horizontal row enough for UX-34? The first cut can use rows with file icons and the new menu;
  a grid is a visual refinement on top.
- Should PDF page rendering cache the last few pages, or re-render on every navigation? The
  first cut can re-render one page at a time; add a small cache only if navigation feels slow.
- Do we want a visible “Open” button in the preview window, or only the tile menu? The
  recommendation is yes: Open / Open With… / Save… belong in the preview action bar.
