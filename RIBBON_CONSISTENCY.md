# Ribbon consistency and compose tabs

Implementation plan for **OUTLOOK_PARITY.md §2.2 item 2** — *Ribbon consistency and compose tabs* (UX-16, UX-21, UX-40, UX-50, UX-66; U.1).

> The item's description predates the shared ribbon work. `source/crates/app/src/ribbon.rs` already implements the shared component, and Mail, Calendar, People, Tasks and the composer already build their command bars through `RibbonBand`. The remaining work is tab/page structure, Outlook's command contents, and the composer's contextual tabs. **Do not rebuild the ribbon component.**

---

## 1. Current state

### Already landed

- `ribbon.rs` provides:
  - `RibbonBand` / `RibbonGroup` with `button`, `small_button`, `toggle`, `menu`, `split`, `widget`;
  - icon+label controls, Simplified and Classic layouts;
  - adaptive label dropping, then the pinned **More** overflow;
  - `RibbonIcon` for live glyphs (Pin, Flag, Read/Unread);
  - `RibbonStyle` shared across every live band, including open composers;
  - Ctrl+F1 collapse.
- Mail Home, Mail's other tabs, Calendar, Tasks, People, Lookout, Search and the composer all call `RibbonBand::new`.
- The existing ribbon content is arranged into Outlook-style groups, but Mail Home's order does not fully match UX-21, and several modules still have only one command page.
- `ComposeActionBar` already moves the composer's editor toolbar between the reading-pane action bar and its pop-out window.

### What is actually missing

1. A reusable tab/page abstraction for module ribbons and composer ribbons.
2. Per-module pages where Outlook requires them (notably People's View tab).
3. Mail Home's final Outlook order and the missing **New mail ▾** and **Print** commands.
4. Calendar's **New event ▾** split button.
5. People's **New contact ▾**, **Add to favorites**, **Add to list** and **View** commands.
6. Composer tabs: **Message / Insert / Format text / Options**.

### Stale documentation

The following should be corrected as work lands, not treated as work to redo:

- §2.2 item 2's claim that the four modules use "three different command-bar styles".
- §2.3's "icon-only ribbon" and "View is greyed out outside Mail" bullets.
- UX-16 / U.1 status once verified.
- UX-21, UX-40, UX-50 and UX-66 once their remaining items ship.

---

## 2. Tasks

| # | Task | Covers | Touches | Effort |
|---|---|---|---|---|
| R1 | **Baseline audit and doc correction** — verify the existing ribbon in a running build; inventory every Lookout-only control and its retained home; add regression coverage for custom commands. Correct stale parity text rather than rebuilding landed work. | U.1, UX-16 | `OUTLOOK_PARITY.md`, `window/tests.rs`, `ribbon.rs` tests | S |
| R2 | **Tab/deck plumbing** — add `RibbonTabs`/`RibbonDeck` helpers to `ribbon.rs`; stop globally disabling `ribbon_tabs` outside Mail; map `(module, tab)` to pages; add the People **View** page; preserve the contextual Search row and composer takeover; make `RibbonOverflow` deck-aware and prune dead bands. | UX-15 (partial), UX-66 (View) | `ribbon.rs`, `window.rs`, `window/calendar_glue.rs`, `window/compose_host.rs` | M |
| R3 | **Mail Home reorder** — add `New mail ▾`, reorder to Outlook's sequence, add **Print**, make Snooze a menu, keep all custom commands. | UX-21 | `window.rs`, `window/mail_actions.rs` | M |
| R4 | **Calendar Home** — make New event a split button (Event, Teams meeting); keep Go To, Schedule/Split/List, Filter, Share and Print; put them in Outlook's order. | UX-50 | `window.rs`, `event_editor.rs`, `crates/graph/src/calendar/` | S |
| R5 | **People Home** — add `New contact ▾` (Contact / Contact list), **Add to favorites**, **Add to list ▾**, Import, Export and a **View** tab for sort / File contacts as; keep Email, Merge duplicates, Manage groups and Open in window. Contact-list creation needs `KIND:group` vCard creation and membership helpers. | UX-66, P-05/P-07 (partial) | `window.rs`, `contacts_view.rs`, `contacts_editor.rs`, `crates/core/src/vcard.rs` | M–L |
| R6 | **Compose contextual tabs** — split the composer band into Message, Insert, Format text and Options; build a compose deck that works embedded and popped out; swap the main module tabs for compose tabs while embedded; omit Draw. | UX-40 | `compose.rs`, `window/compose_host.rs`, `window.rs`, `ribbon.rs` | L |
| R7 | **Compose control mapping** — move every existing control into the right tab; use shared `gio::SimpleAction`s for commands that appear on more than one tab; register composer bands with `RibbonOverflow`; keep draft autosave, file drop, attachment reminder, undo send and pop-out unchanged. | UX-40 | `compose.rs`, `ribbon.rs`, `window/compose_host.rs` | M |
| R8 | **Verification, tests, docs and release** — formatter, check, clippy and tests; GTK tests for pages, install/restore/pop-out, overflow and narrow widths; manual Simplified/Classic/Ctrl+F1 matrix; update parity doc, changelog, version and packaging metadata. | all | repo-wide | S |

### Suggested order

```text
R1 → R2 → (R3 ∥ R4 ∥ R5 ∥ R6/R7) → R8
```

R6 is the critical path and can start once R2 lands. R3–R5 are independent.

**Estimated total:** roughly 2–4 weeks. Compose tabs and People contact-list support are the two largest pieces.

---

## 3. Task detail

### R1 — Baseline audit and doc correction

- Launch the current build and inspect Mail, Calendar, People, Tasks and both composer states (embedded and popped out).
- Confirm every module and tab uses `RibbonBand`; record any control still outside it.
- Write down the Lookout-only commands and where they will live after the reorder.
- Extend the ribbon/window tests so custom commands cannot silently disappear.
- Update the stale claims in `OUTLOOK_PARITY.md` §2.2, §2.3, UX-16 and U.1 only after the verification passes.

**Acceptance:** the plan's retention inventory is reflected in tests, and the parity document no longer asks for work that has already landed.

### R2 — Tab/deck plumbing

The existing `view_toolbar_stack` already switches pages, but the tab strip is bespoke and disabled outside Mail, and the composer has no page abstraction.

Proposed work:

- Add a reusable `RibbonTabs` (or equivalent) in `ribbon.rs`:
  - a mutual-exclusion group of `.ribbon-tab` toggle buttons;
  - accessible names and keyboard focus;
  - `add_tab(label, page_name)`, `set_active(page)`, `set_available(&[...])`;
  - a change callback that switches a supplied `gtk::Stack`.
- Add a `RibbonDeck` that owns a stack of `RibbonBand`s and their tab strip. Use it for the composer; migrate the main window's tab strip to it if the refactor stays contained.
- Extend `window/calendar_glue.rs::ribbon_stack_name` with per-module pages:
  - Mail: `mail-home`, `mail-send-receive`, `mail-folder`, `mail-view`, `mail-help`;
  - Calendar: `calendar` (and later `calendar-view`);
  - People: `contacts`, `contacts-view`;
  - Tasks: `tasks` (and later `tasks-view`);
  - Lookout: `lookout`.
- Stop calling `ribbon_tabs.set_sensitive(false)` for Calendar, People, Tasks and Lookout. Instead:
  - hide **Send / Receive** and **Folder** outside Mail;
  - show **Home**, **View** (where implemented), **Help**;
  - select a valid page when switching modules;
  - preserve the previous Mail tab.
- Preserve the contextual **Search** page (`state.search_ribbon`) and its restoration on exit.
- Make `RibbonOverflow` work when the visible child is a deck rather than a band:
  - resolve the deck's active page;
  - register the deck's bands;
  - prune dead registrations when composers close.
- Keep the pinned **More** button, Layout control and Ctrl+F1 collapse working on every page.

**Acceptance:**

- One tab mechanism drives module and composer pages.
- Switching modules or opening/closing a composer never leaves an empty or wrong page.
- The Search row still appears and disappears correctly.
- A composer's bands overflow like every other band.

### R3 — Mail Home reorder (UX-21)

Target order, with custom commands retained:

```text
New mail ▾
Delete | Archive | Report ▾ | Sweep
Move to ▾ | Rules ▾
Reply | Reply all | Forward ▾ | Meeting | Add Sender
Read/Unread | Categorize ▾ | Flag ▾ | Pin | Snooze ▾
Quick steps ▾
Print
Find: Filter Email
Send/Receive: Send/Receive All Folders
More
```

Work items:

- Replace the plain **New mail** button with a split button:
  - main action: New mail, using the existing `compose_button` path;
  - menu: New event, New contact, New task.
- Prefer shared window actions (`win.new-mail`, `win.new-event`, `win.new-contact`, `win.new-task`) so the split menu, tray and future keyboard preset use one path.
- Reorder the existing groups; do not remove any control.
- Add a visible **Print** command wired to the existing `print_reading_pane` path (the **More** menu's Print entry stays).
- Convert Snooze from a plain button with a popover to a menu-shaped control so its dropdown affordance is visible.
- Keep **Forward ▾**'s Forward-as-attachment entry.
- Keep the More extras (View details/source, Open in new window, Save as `.eml`, Print, etc.).

**Acceptance:** the Home band visually matches UX-21, and every pre-existing Mail action remains reachable with the same handler and undo behaviour.

### R4 — Calendar Home (UX-50)

- Replace **New event** with a split button:
  - **Event** opens the existing `show_new_event_editor` path;
  - **Teams meeting** opens the same editor with an online-meeting prefill.
- Show **Teams meeting** only when the target calendar is a Microsoft 365 Graph calendar that can create an online meeting. Leave CAL-20's general Teams toggle out of this item.
- Keep the existing **Go To** group (Today, Back, Forward, Go to Date) as a Lookout extra.
- Keep **Schedule**, **Split view** and **List** alongside Outlook's Day / Work week / Week / Month.
- Order:

```text
New event ▾
Day | Work week | Week | Month | Schedule | Split view | List
Go To
Filter
Share
Print
```

- Do not create an empty Calendar **View** tab. UX-54's time scale, colour mode, week numbers, split orientation and overlay options belong with U.7/CAL-04 once those controls exist.

**Acceptance:** Event and Teams meeting open the correct editor, the Teams item is hidden for non-Graph accounts, and all previous Calendar commands remain.

### R5 — People Home (UX-66)

Target Home band:

```text
New contact ▾ | Edit | Delete
Add to favorites | Add to list ▾
Import | Export
View: Sort / File contacts as
Custom: Email | Merge duplicates… | Manage groups… | Open in window
```

Work items:

- **New contact ▾**
  - **Contact** uses `show_new_contact_editor`.
  - **Contact list** creates a `KIND:group` vCard in a writable CardDAV book.
  - Hide contact-list creation for Graph books (Graph v1.0 has no contact-group endpoint) and read-only/desktop books.
- **Add to favorites**
  - Toggle `UiState::starred_contacts` and `ui_state_db::set_starred` for the checked contacts.
  - Refresh Favourites and the row star.
- **Add to list ▾**
  - List the account's contact lists (`KIND:group` cards).
  - Add the selected contacts by UID and/or address, preserving existing members.
  - Update the group card through the owning session with its etag.
  - Hide or disable when no writable lists exist.
- **View tab**
  - Expose the existing "File contacts as" sort (`CONTACTS_FILE_AS`), wired to `contacts_list_refresh`.
  - Keep the existing Filter/Sort popover untouched; both surfaces should share the same setting and repaint path.
- **Retain custom commands**
  - Email → Communicate group.
  - Merge duplicates… → Actions group.
  - Manage groups… → Groups group.
  - Open in window → trailing standalone control.

Add pure helpers in `crates/core/src/vcard.rs` (or a small companion module) for:

- building a contact-list card;
- adding/removing `X-ADDRESSBOOKSERVER-MEMBER` / `MEMBER` references;
- deduplicating members.

**Acceptance:** a user can create a contact list, add/remove members on CardDAV, star contacts from the ribbon, and sort People from the View tab; Graph and desktop books degrade by hiding unsupported actions, never by erroring.

### R6/R7 — Compose contextual tabs (UX-40)

#### Tabs

| Tab | Contents |
|---|---|
| **Message** | Send ▾ (Send now / Schedule send), Attach file, Signature ▾, High/Low importance, Save draft, Close |
| **Insert** | Picture, Table, Link, Emoji, Horizontal line, Signature ▾, My Templates |
| **Format text** | Font family/size, colour, bold/italic/underline/strikethrough, bullets/numbering, indent/outdent, align left/centre/right/justify, clear formatting |
| **Options** | Importance, Sensitivity, Request read receipt, Delay delivery (Schedule send), Switch to plain text / Rich text |

- Draw is out of scope.
- Spell check stays on F7 and the global setting.
- Draft autosave, file drop, attachment reminder, undo send and existing send/discard behaviour are unchanged.

#### Architecture

- Build one `RibbonBand` per composer tab.
- Return a composer deck (tab strip + page stack) from `build_compose_view`, or attach it to the existing `editor_toolbar`.
- In the recommended design, `ComposeActionBar`:
  - hides the main module tabs while the composer owns the `mail-home` slot;
  - shows the composer's Message / Insert / Format text / Options tabs in their place;
  - restores the previous module tab and page when the composer closes or pops out;
  - keeps **File** visible.
- In the pop-out window, the composer deck carries its own tab strip so the tabs remain usable.
- `PopOutHandle` must carry the deck/tab handles through the existing move sequence:
  - embedded → pop-out;
  - pop-out → pop-back-in;
  - send/discard while popped out.
- `ComposeActionBar::restore` must keep its identity check so a finished composer cannot steal another composer's slot.

#### Shared actions instead of duplicate widgets

GTK widgets have one parent. Commands that appear on more than one tab need separate controls bound to one shared action:

- Signature (Message + Insert)
- Importance (Message + Options)
- Schedule send (Message + Options)
- Rich/plain toggle (Options + Format text if duplicated)

Prefer a composer `gio::SimpleActionGroup` for these rather than a second stateful widget.

#### Overflow

- Register every composer band with `RibbonOverflow` when the deck is installed.
- Resolve the deck's active page for overflow measurement.
- Prune the registration when the composer closes so long sessions do not accumulate dead bands.
- Verify labels drop before controls move into **More**, exactly as on the main ribbon.

**Acceptance:**

- Both embedded and popped-out composers show the four tabs.
- Every control from the current single bar is reachable exactly once.
- Switching Simplified/Classic or collapsing the ribbon affects all composer tabs.
- Narrow widths drop labels and move controls into More without clipping.
- Send, schedule send, draft autosave, pop-out and pop-back-in still work.

### R8 — Verification, docs and release

- `cargo fmt --all -- --check`.
- `cargo check -p lookout-app --all-targets`.
- `cargo clippy -p lookout-app --all-targets -- -D warnings`.
- Targeted tests plus the app suite.
- GTK tests:
  - each module/tab resolves to the right page;
  - People View tab toggles;
  - composer tab switching;
  - composer install/restore/pop-out/pop-back-in;
  - overflow from each composer page;
  - narrow-width label drop.
- Manual matrix:
  - Mail, Calendar, People, Tasks, Lookout;
  - Simplified and Classic;
  - Ctrl+F1 collapsed and expanded;
  - wide and narrow windows;
  - composer embedded, popped out, and back;
  - search active/inactive;
  - no selection / multiple selection.
- Accessibility: every new tab and control has an accessible name and a tooltip that follows `shortcuts.rs`.
- Update `OUTLOOK_PARITY.md`:
  - §2.2 item 2;
  - §2.3;
  - UX-16, UX-21, UX-40, UX-50, UX-66 and U.1;
  - UX-15's View-tab note if People's View tab lands.
- Add a `CHANGELOG.md` entry citing the IDs.
- Bump the workspace version and packaging metadata following the repository's existing release convention.

---

## 4. Retention inventory

Do not remove or rename these while rearranging the ribbon.

| Surface | Keep |
|---|---|
| **Mail Home** | Sweep, Report ▾, Move to ▾, Rules ▾, Meeting, Add Sender, Quick steps ▾, Categorize ▾, Flag ▾, Pin, Snooze ▾, Filter Email, Send/Receive All Folders, Forward ▾ (including Forward as attachment), all **More** extras |
| **Calendar Home** | Go To (Today/Back/Forward/Go to Date), Schedule, Split view, List, Share, Print |
| **People Home** | Email, Merge duplicates…, Manage groups…, Open in window |
| **Tasks Home** | New task, Sort & group, Connect Google Tasks |
| **Compose** | My Templates, Signature, Save draft, Close, rich/plain, request read receipt, importance, sensitivity, schedule send, table, link, image, emoji, horizontal rule, all formatting controls |
| **Every module** | Simplified/Classic switch, Ctrl+F1 collapse, trailing More overflow, Layout control, shortcut tooltips |

Commands may move between groups or tabs, but each must keep its existing handler path (or be bound to an equivalent shared action) so behaviour, sensitivity and undo stay unchanged.

---

## 5. Risks and mitigations

| Risk | Mitigation |
|---|---|
| The parity text is stale and invites rebuilding a working ribbon | R1 corrects the document first; U.1/UX-16 are closed as verification, not reimplementation |
| GTK single-parent rule breaks duplicated commands | Use shared `gio::SimpleAction`s; never reparent one button across tabs |
| Composer toolbar moves between reading pane, action bar and pop-out window | Keep one owner (`ComposeActionBar`/composer deck), preserve identity guards, test every transition |
| `RibbonOverflow` assumes the visible child is a band | Add deck-aware active-page resolution and prune dead bands |
| Tabs and Search/composer takeover conflict | Keep one controller for "which page is visible"; restore the previous module tab on exit |
| Empty View tabs appear for unimplemented features | Only add tabs with real content; defer Calendar UX-54 and Tasks UX-74 controls until those features exist |
| Graph cannot store contact groups or create Teams links on every account | Hide unsupported actions per account/capability; degrade softly (§10) |
| Custom Lookout commands are lost during reordering | Retention inventory in R1, asserted by tests, rechecked in R8 |

---

## 6. Open decisions

1. **People contact lists** — full "New contact list" and "Add to list" support is needed to close UX-66 completely. If a smaller first pass is preferred, ship the split menu, favourites and View tab first and leave UX-66 partial until the contact-list work lands.
2. **Calendar and Tasks View tabs** — UX-54 and UX-74 are separate roadmap items. This plan adds only the People View tab, because UX-66 requires one. Calendar and Tasks View tabs should wait until their controls exist rather than shipping empty tabs.
3. **Compose tab strip placement** — swapping the main module tab strip while composing is closest to Outlook. A composer-owned strip inside the command area is simpler but visually less exact. Recommended: swap the main strip and use the composer-owned strip only in the pop-out window.
