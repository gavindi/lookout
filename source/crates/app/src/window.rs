/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use chrono::{Datelike, Timelike};
use gtk::{gio, glib};
use lookout_core::{
    display_name, AccountId, Attendee, AttendeeRole, AttendeeStatus, BodyPart, CalendarEvent, CalendarId, CalendarInfo, CalendarTask, ContactsProvider, EmailAddress, EmailBody,
    EmailSummary, EventOccurrence, EventUid, Mailbox, MailboxId, MailboxRole, SystemFlagBit, TaskPriority, TaskStatus, TaskUid, Uid, VCard, WebcalSubscription,
};
use lookout_dav::session::{CalendarCommand, CalendarSessionEvent, ConnectionState as CalConnectionState};
use lookout_dav::subscription::{SubscriptionCommand, SubscriptionSessionEvent};
use lookout_dav::CalendarAccountConfig;
use lookout_goa::{GoaCalendarAccount, GoaClient};
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{AccountConfig, EndpointConfig};
use webkit::prelude::*;

use crate::calendar_colors;
use crate::calendar_view::{self, CalendarMain};
use crate::contacts_view::{
    calendar_attendee_suggestions, export_current_contacts, find_contact_by_address, merge_contact_suggestions, rebuild_contacts_list_ui, refresh_contacts_category_ui,
    show_contact_details_dialog, show_contact_editor_for, show_contacts_import_dialog, show_create_contact_for, show_manage_groups_dialog, show_new_contact_editor,
    spawn_contacts_discovery, sync_contacts_account, ContactCommand, ContactsAccountSnapshot, ContactsCategoryChoice, ContactsListEntry, SnapshotContactsProvider,
};
use crate::folder_tree::{build_multi_account_tree_model, TreeItem};
use crate::goa_calendar_credentials::GoaCalendarCredentialProvider;
use crate::goa_credentials::GoaCredentialProvider;
use crate::google_tasks::{self, GoogleTasksCommand, GoogleTasksEvent, TaskList};
use crate::last_view::{self, LastSelection};
use crate::message_list::{format_row_date, unified_merge_order, ListFilter, MessageItem, MessageListModel, SelectionKind, SortKey};
use crate::microsoft_oauth::MicrosoftCredentialProvider;
use crate::ui_state_db::UiStateDb;
use crate::worker::Worker;

/// Per-account state the UI needs once an `AccountSession` actor is running:
/// how to send it commands, its identity (for compose "From" and toast
/// labeling), and the last folder list it reported (kept here so the
/// multi-account folder tree can be rebuilt in full from all accounts'
/// latest snapshots whenever any one of them changes).
/// `pub(crate)` because `contacts_view` reads `cmd_tx`/`email`/`display_name`
/// (and `address_cache`) off the account handles.
pub(crate) struct AccountHandle {
    pub(crate) cmd_tx: async_channel::Sender<AccountCommand>,
    pub(crate) email: String,
    pub(crate) display_name: String,
    /// Connection parameters, kept for the Config view's account overview
    /// (the Config view shows how each account is configured, not just that
    /// it exists).
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    folders: Vec<Mailbox>,
    /// Read-side handle on this account's cache, used for folder-switch/
    /// search/composer-autocomplete reads. Deliberately a second connection
    /// to the file the session writes: routing a lookup through
    /// `AccountCommand` would put every keystroke behind whatever IMAP round
    /// trip the session is mid-way through. The cache opens WAL for exactly
    /// this, and a failed open just means no suggestions. `pub(crate)` for
    /// `contacts_view`'s attendee autocomplete, which unions the mail-history
    /// caches across every connected account. `Arc` rather than `Rc`: reads
    /// off it are dispatched onto the `Worker`'s thread pool via
    /// `spawn_cache_read`, and `Cache` is already `Send + Sync` (its
    /// connection is `Mutex`-guarded), so `Arc` is what makes that possible.
    pub(crate) address_cache: Option<Arc<lookout_mail::Cache>>,
}

/// One GNOME Online Accounts account as discovered at startup, keeping the
/// raw GOA structs so a disabled account can be reconnected from Config
/// without re-running discovery. Each of the three services the account
/// advertises (Mail/Calendar/Contacts) is stored under its own field;
/// Google Tasks keys off the calendar entry's display name (the email).
/// `pub(crate)` because `contacts_view` inserts contacts entries into it.
#[derive(Clone)]
pub(crate) struct DiscoveredGoaAccount {
    pub display_name: String,
    pub email: String,
    pub provider_type: Option<String>,
    pub mail: Option<lookout_goa::GoaMailAccount>,
    pub calendar: Option<GoaCalendarAccount>,
    pub contacts: Option<lookout_goa::GoaContactsAccount>,
}

/// What the message list is currently showing - either a single mailbox (the
/// classic folder-selection view), the synthetic "All Inboxes" unified view
/// merging every connected account's Inbox, or full-text search results.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MailView {
    Single,
    UnifiedInbox,
    /// Full-text search results across the local FTS index plus the live IMAP
    /// pass on the open mailbox. Only ever active with a non-empty
    /// `UiState::search_query`; exiting search restores the pre-search view.
    Search,
}

/// Every widget one message-list row owns, stashed on the `Gtk.ListItem` at
/// setup so `bind` can address them by name instead of walking the widget
/// tree with a chain of `first_child()`/`last_child()` downcasts - which the
/// two-branch row below is far too deep for.
///
/// Stored under a single `set_data` key: the `set_data`/`data` pair is
/// `unsafe` and reads back whatever type you name, so one struct under one
/// key keeps the two ends impossible to mismatch.
#[derive(Clone)]
struct MessageRowWidgets {
    header_box: gtk::Box,
    expander: gtk::TreeExpander,
    header_label: gtk::Label,
    message_box: gtk::Box,
    /// The conversation-header row: an expander over the same sender /
    /// subject / date line a message row shows, plus the participant count.
    /// Built in `connect_setup` and toggled in `bind` like the other two row
    /// kinds (see the `header_box`/`message_box` notes there).
    thread_box: gtk::Box,
    thread_expander: gtk::TreeExpander,
    thread_sender: gtk::Label,
    /// Attachment/calendar indicator icons on the conversation-header row,
    /// sitting right after the sender column exactly like the message row's
    /// own pair.
    thread_attachment_icon: gtk::Image,
    thread_calendar_icon: gtk::Image,
    thread_subject: gtk::Label,
    thread_count: gtk::Label,
    thread_flag: gtk::Image,
    thread_date: gtk::Label,
    /// Batch-select checkbox, kept in sync with the row's real selection
    /// state (see `checkbox_suppress`'s note) rather than driving it
    /// directly - clicking it and ctrl/shift-clicking the row are two inputs
    /// to the same `MultiSelection`.
    checkbox: gtk::CheckButton,
    /// Set while `bind()`/the model's `notify::selected` handler are the
    /// ones writing `checkbox.set_active()`, so `checkbox`'s own
    /// `connect_toggled` (installed once in `connect_setup`) can tell a
    /// programmatic sync apart from a real click and not feed it back into
    /// the selection model - the same pattern as `ListHeader::favorite_suppress`.
    checkbox_suppress: Rc<Cell<bool>>,
    accent: gtk::Box,
    avatar: gtk::Label,
    sender_label: gtk::Label,
    /// Attachment/calendar indicator icons, right after the sender column.
    /// Hidden on rows that have neither, so they take no width at all there -
    /// the same discipline as `flag_icon`, and the fixed sender column keeps
    /// their position aligned across rows.
    attachment_icon: gtk::Image,
    calendar_icon: gtk::Image,
    subject_label: gtk::Label,
    flag_icon: gtk::Image,
    /// The color-tag dots: one small circle per configured tag the message
    /// carries, rebuilt on every bind.
    tag_dots: gtk::Box,
    date_label: gtk::Label,
    preview_label: gtk::Label,
    action_box: gtk::Box,
    /// The right-click context menu's popover, parented to this row in setup
    /// and repopulated at press time.
    tag_popover: gtk::Popover,
    /// The message this row currently shows. Set by `bind`, read by the
    /// quick-action handlers when they fire.
    bound: Rc<RefCell<Option<EmailSummary>>>,
}

/// Per-row widgets for the folder tree rows, stored under a `set_data` key on
/// the row's `TreeExpander` (same pattern as `MessageRowWidgets` under
/// `row-widgets`): `bind` refreshes which folder the expunge button acts on
/// and whether the row is eligible at all, and the click handler reads the
/// target at click time.
#[derive(Clone)]
struct FolderRowWidgets {
    action_box: gtk::Box,
    expunge_btn: gtk::Button,
    /// Which mailbox the expunge button acts on - `None` on rows that aren't
    /// Trash/Junk (where the button never shows anyway).
    expunge_data: Rc<RefCell<Option<Mailbox>>>,
    /// Whether this row's folder can be expunged; gates the hover reveal so
    /// ordinary folders never flash an empty action box.
    expunge_enabled: Rc<Cell<bool>>,
}

/// How many recently-viewed message bodies to keep in memory. Every message
/// switch used to re-fetch the whole body over IMAP - there was no cache
/// beyond the single currently-open message - so flipping between emails (or
/// re-opening one already read) re-downloaded everything, network round trip
/// included. This LRU makes a revisit render instantly; `lookout-mail`'s disk
/// cache covers the cross-restart and long-session cases.
const BODY_CACHE_IN_MEMORY: usize = 25;

/// If WebKit hasn't reported `Finished` for a body load within this long, the
/// reading pane reveals the page anyway instead of sitting on the empty
/// placeholder. Revealing is normally gated on `Finished` so the fade-in
/// never shows a blank page, but a slow/hung load (e.g. a message referencing
/// remote resources) must not hold the pane blank indefinitely.
const HTML_REVEAL_TIMEOUT_MS: u64 = 400;

/// How narrow the mail screen's folder pane may be dragged, in pixels. The
/// pane holds a `Gtk.ScrolledWindow`, which reports no meaningful minimum
/// width of its own, so without this the separator can be dragged until the
/// folder names are a sliver. Applied to `folder_card`, not the scroller -
/// see the call site.
const FOLDER_PANE_MIN_WIDTH: i32 = 200;

/// How wide the mail screen's folder pane may be, in pixels. The separator
/// stops here no matter how wide the window grows, so the pane never becomes
/// an unreadably wide column.
const FOLDER_PANE_MAX_WIDTH: i32 = 320;

/// How long after the last keystroke before a search query is committed. Long
/// enough that a burst of typing runs one search, not one per keypress - the
/// live IMAP pass costs a round trip, and even the FTS pass re-renders the
/// list - but short enough that results feel immediate.
const SEARCH_DEBOUNCE_MS: u32 = 300;

/// How many hits each source contributes to a search: the FTS cache pass and
/// the live IMAP pass each cap their answer so a search over a large mailbox
/// doesn't hand the list an unbounded set to rebuild from. Generous - the
/// user can refine the query to narrow further.
const SEARCH_RESULT_LIMIT: usize = 300;

/// How many color-tag dots one message row renders. A message can carry any
/// number of tags; past this the row draws the first few and leaves the rest
/// to the Categorize menu, so a heavily-tagged row doesn't crowd out the date.
const MAX_TAG_DOTS: usize = 3;

/// How long a Save-button attachment fetch may take before the UI gives up
/// and restores the button. The session answers every `FetchAttachment`
/// (with `PartFetched` or `PartFetchFailed`) except when the connection dies
/// mid-fetch and the command is lost to the reconnect; this timeout is the
/// backstop that guarantees the button can never be stuck on "Fetching…"
/// forever. Generous: the fetch is a single IMAP literal, and multi-megabyte
/// attachments over slow connections legitimately take a while.
const ATTACHMENT_FETCH_TIMEOUT_MS: u64 = 60_000;

/// How many session events may queue before the session stalls on send. The
/// UI drains each loop's channel in batches and collapses whole-snapshot
/// events into the last copy of each (see `collapse_last_wins`), so a full
/// channel is a transient burst - and backpressure is the point: an unbounded
/// channel lets a sync storm grow memory without bound while the UI serially
/// repopulates. The UI -> session *command* channels stay unbounded on
/// purpose: commands are sent with `send_blocking` from the GTK main thread,
/// and the session can be mid-fetch for seconds, so a bounded command channel
/// would freeze the UI on click.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// The "Switch message theme" toggle's stylesheet, injected as a WebKit
/// *user* style sheet into the reading pane's WebView: it strips whatever
/// background colour the email itself defines so the app theme's background
/// shows through instead, and inverts the document's colours so the content
/// still reads against that (mostly darker) theme background. `html`/`body`
/// covers the page background proper (inline styles, `<style>` blocks, or
/// `<body bgcolor>`), while the attribute selectors catch the legacy
/// `bgcolor`/`background` attributes Outlook-style clients plaster onto
/// wrapper tables and cells - the common full-bleed case that an
/// `html, body` override alone would miss. Deliberate backgrounds on content
/// (code blocks, quoted sections) are left alone. The `!important` is what
/// wins: user-level `!important` outranks both author `!important` and
/// inline styles in the cascade, so no email CSS can fight the override.
/// Injected at the user level so it applies to the already loaded document
/// the moment the toggle flips (plus a re-render, see the toggle's handler),
/// and applies to every `load_html` while it's armed.
///
/// The inversion is the standard reader-mode trick: `filter: invert()`
/// flips the whole document (dark text becomes light, coloured headings and
/// links keep a readable hue via the compensating `hue-rotate`), then media
/// elements are inverted a second time so photos and inline graphics render
/// as themselves instead of negatives. Because the email's own backgrounds
/// are gone above, only the text and any remaining accent colours invert -
/// against the app theme's background, which is untouched by the page's
/// filter.
const MESSAGE_THEME_OVERRIDE_CSS: &str = "\
html, body {
    background-color: transparent !important;
    background-image: none !important;
}
*[bgcolor] {
    background-color: transparent !important;
}
*[background] {
    background-image: none !important;
}
html {
    filter: invert(1) hue-rotate(180deg);
}
html img, html picture, html video, html svg, html canvas {
    filter: invert(1) hue-rotate(180deg);
}
";

/// Which of an attachment row's actions a `PendingAttachment` fetch was for -
/// decides what happens to the bytes once `AccountEvent::PartFetched` lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAttachmentAction {
    /// Open the bytes with the MIME type's *default* application, resolved via
    /// GIO and launched directly (no chooser), from a temporary file deleted
    /// when Lookout exits. Falls back to the XDG portal when there's no
    /// default app for the type (or the direct launch fails, e.g. sandboxed).
    Open,
    /// Ask the user which application to open the file with, via the XDG
    /// desktop portal's `org.freedesktop.portal.OpenURI` (the `ask` option).
    OpenWith,
    /// Offer a save-location dialog and write the bytes there.
    Save,
}

/// One attachment action in flight for the reading pane, tracked in
/// `UiState::pending_attachment`. `button` is the strip row's menu button,
/// disabled while the fetch is outstanding so the user can see which row is
/// preparing; it's restored on completion.
struct PendingAttachment {
    mailbox: MailboxId,
    uid: Uid,
    /// The `BodyPart::part_number` being fetched - matched against a late
    /// `PartFetched` so a response meant for a different part (or a different
    /// message) can't re-enable the wrong row.
    part_number: String,
    /// What to do with the bytes once they arrive.
    action: PendingAttachmentAction,
    /// The row's menu button, to re-enable once the fetch lands.
    button: gtk::MenuButton,
}

/// One .eml export in flight for the reading pane's "More" menu, tracked in
/// `UiState::pending_raw_message`. `initial_name` is the save dialog's
/// suggested filename (built from the message's subject at request time, so
/// it survives even if the selection changes before the bytes arrive).
struct PendingRawMessage {
    mailbox: MailboxId,
    uid: Uid,
    initial_name: String,
}

/// Marks a value as `Send` so it can cross from WebKit's scheme-handler
/// thread to the main loop. `webkit::URISchemeRequest` is a ref-counted
/// GObject whose `finish`/`finish_error` are documented thread-safe; the
/// wrapper exists only because the gtk-rs bindings don't declare it `Send`.
/// The request is only ever *touched* on the main thread after arrival -
/// the same pattern as the credential providers' `SendWrapper` in
/// `build_window`, where the justification is spelled out in full.
struct SendWrapper<T>(T);
unsafe impl<T> Send for SendWrapper<T> {}

/// A `cid:` image request forwarded from WebKit's scheme-handler thread to
/// the main loop: `cid` is the reference as it appeared in the message's
/// HTML, and `request` is the WebKit request that must be finished with the
/// part's bytes (or an error) once the main loop has resolved it.
struct CidSchemeRequest {
    request: SendWrapper<webkit::URISchemeRequest>,
    cid: String,
}

/// One inline `cid:` image fetch in flight for the message on the reading
/// pane, tracked in `UiState::pending_cid` and keyed by the `BodyPart`'s
/// part number. Answered with the part's bytes (`AccountEvent::PartFetched`)
/// or an error (`PartFetchFailed`, a timeout, or the user moving to another
/// message); `request` is the WebKit URI-scheme request the answer is
/// finished into. The part itself isn't kept - the response arms re-read it
/// from the event.
struct PendingCid {
    mailbox: MailboxId,
    uid: Uid,
    request: webkit::URISchemeRequest,
}

/// A small bounded LRU of recently-viewed message bodies, keyed by
/// `(mailbox, uid)`, front = most recently used. See `BODY_CACHE_IN_MEMORY`.
struct BodyCache {
    capacity: usize,
    entries: VecDeque<(MailboxId, Uid, EmailBody)>,
}

impl BodyCache {
    fn new(capacity: usize) -> Self {
        BodyCache {
            capacity,
            entries: VecDeque::new(),
        }
    }

    /// Returns the cached body for `(mailbox, uid)`, promoting it to most
    /// recently used, or `None` on a miss. O(capacity) - the cache is tiny,
    /// and bodies are cloned out anyway so the scan is not the cost.
    fn get(&mut self, mailbox: &MailboxId, uid: &Uid) -> Option<EmailBody> {
        let pos = self.entries.iter().position(|(m, u, _)| m == mailbox && u == uid)?;
        let entry = self.entries.remove(pos).expect("position came from this deque");
        self.entries.push_front(entry);
        Some(self.entries.front().expect("just pushed").2.clone())
    }

    fn insert(&mut self, mailbox: MailboxId, uid: Uid, body: EmailBody) {
        if let Some(pos) = self.entries.iter().position(|(m, u, _)| *m == mailbox && *u == uid) {
            self.entries.remove(pos);
        }
        self.entries.push_front((mailbox, uid, body));
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
    }
}

/// Mutable UI-thread state the various signal handlers close over. Plain
/// `Rc<RefCell<_>>` is fine here - GTK is single-threaded, so there's no
/// need for `Arc<Mutex<_>>` on this side of the worker-thread boundary.
/// `pub(crate)` because `contacts_view` (the People screen module) reads and
/// writes the contacts fields below from its own file.
pub(crate) struct UiState {
    /// Per-account mail session handles. `pub(crate)` for `contacts_view`,
    /// whose attendee autocomplete unions the mail-history caches across
    /// every connected account.
    pub(crate) accounts: HashMap<AccountId, AccountHandle>,
    /// CardDAV-derived contacts discovered per account, including both
    /// category buckets (for Contacts UI) and flattened suggestions (for
    /// composer autocomplete). Owned by the People screen module
    /// (`contacts_view`), read by the composer/attendees here.
    pub(crate) contacts_by_account: HashMap<AccountId, ContactsAccountSnapshot>,
    /// Contacts starred via the People screen's row toggle, keyed by
    /// `(account, contact_identity)`. Local-only: never written back to the
    /// vCard or synced to the server, so it resets if the identity a contact
    /// resolves to (its `UID`, falling back to its first email) changes.
    /// Persisted in the UI-state database (`ui_state_db`), loaded at startup
    /// and written through on every star toggle.
    pub(crate) starred_contacts: HashSet<(AccountId, String)>,
    /// The UI-state database backing `starred_contacts`, opened best-effort
    /// at startup. `None` when the database couldn't be opened - favourites
    /// then fall back to session-only, never an error.
    pub(crate) ui_db: Option<Rc<crate::ui_state_db::UiStateDb>>,
    /// The GSettings-backed preference store (see `settings`), resolved once
    /// in `build_window` and written through on every preference change.
    settings: Rc<crate::settings::SettingsStore>,
    /// The relational-data config (`settings.json`): sending identities and
    /// folder-role overrides. Loaded at startup, written through by the
    /// manage-identities dialog. See `app_config`.
    app_config: Rc<RefCell<crate::app_config::AppConfig>>,
    /// Contacts that were present in a previous CardDAV sync for an account
    /// but are missing from the latest one - the People screen's Deleted
    /// bucket. Filled by `contacts_view`'s sync diff; survives restarts
    /// because the first poll of a session diffs against the cached
    /// snapshot, so deletions made while the app was closed land here too.
    pub(crate) deleted_contacts: HashMap<AccountId, Vec<VCard>>,
    /// Per-account command channel into the CardDAV poll loop
    /// (`sync_contacts_account`) - the write path for the People screen's
    /// create/edit/delete/import flows. Keyed by account, inserted as each
    /// account's session starts in `spawn_contacts_discovery`.
    pub(crate) contact_cmd_tx: HashMap<AccountId, async_channel::Sender<ContactCommand>>,
    /// Every GOA account discovered at startup, keyed by account id -
    /// including disabled ones, so Config's account list (and a re-enable)
    /// can refer to them without re-running discovery. Populated by the
    /// mail/calendar/contacts discovery passes; the config view reads it and
    /// the enable toggle reconnects from the stored structs.
    pub(crate) goa_accounts: HashMap<AccountId, DiscoveredGoaAccount>,
    /// One GOA D-Bus handle kept from the mail discovery pass, reused to
    /// reconnect a re-enabled account without opening another session-bus
    /// connection. `None` until mail discovery runs (or fails).
    pub(crate) goa_client: Option<GoaClient>,
    /// Which account owns the currently-open mailbox - drives command
    /// routing (FetchBody, compose "From") and which account's
    /// `MessagesUpdated` events are allowed to update the message list (a
    /// background IDLE resync on some *other* account must not clobber
    /// whatever the user is currently looking at).
    current_account: Option<AccountId>,
    current_mailbox: Option<MailboxId>,
    /// Which of the two message-list views is active. `Single` keys the list
    /// off `current_mailbox` exactly as before; `UnifiedInbox` instead merges
    /// every connected account's Inbox (see `unified_snapshots`).
    mail_view: MailView,
    /// The latest per-mailbox message sets for the unified view, filled from
    /// each account's `MessagesUpdated` events while `mail_view` is
    /// `UnifiedInbox`. The visible list is the union of these, deduplicated
    /// by `(mailbox, uid)` and sorted newest-first.
    unified_snapshots: HashMap<MailboxId, Vec<EmailSummary>>,
    /// Rows hidden from the message list on an optimistic delete/archive/
    /// report-as-junk, keyed by source mailbox, kept around so a matching
    /// `AccountEvent::MoveFailed` can restore exactly these rows. Cleared for
    /// a mailbox the moment any `AccountEvent::MessagesUpdated` lands for it -
    /// an authoritative sync always supersedes the optimistic stash.
    pending_optimistic_removals: HashMap<MailboxId, Vec<EmailSummary>>,
    /// Pre-toggle summaries for an optimistic mark-read/unread, keyed by
    /// source mailbox, kept around so a matching
    /// `AccountEvent::StoreFlagsFailed` can restore exactly their original
    /// flags. Cleared for a mailbox the moment any
    /// `AccountEvent::MessagesUpdated` lands for it, same convention as
    /// `pending_optimistic_removals`.
    pending_optimistic_flag_changes: HashMap<MailboxId, Vec<EmailSummary>>,
    /// The most recently requested body fetch, used to ignore stale
    /// `BodyFetched` updates that arrive after the user has moved on to a
    /// different message.
    pending_body_request: Option<(MailboxId, Uid)>,
    /// An attachment-part fetch currently in flight for the reading pane's
    /// row - its menu button is disabled while the bytes are coming, and
    /// re-enabled when `AccountEvent::PartFetched` lands (or discarded if the
    /// user navigates away first). One at a time; the strip's buttons ignore
    /// a click while one is outstanding. See `PendingAttachment`.
    pending_attachment: Option<PendingAttachment>,
    /// A whole-message .eml export currently in flight for the "More" menu -
    /// one at a time, cleared when `AccountEvent::RawMessageFetched`/
    /// `RawMessageFetchFailed` lands (or discarded if the user navigates
    /// away first, in which case the late response is dropped as stale). See
    /// `PendingRawMessage`.
    pending_raw_message: Option<PendingRawMessage>,
    /// Inline `cid:` image fetches in flight for the message on the reading
    /// pane, keyed by `BodyPart::part_number`. A message can embed several
    /// images, so - unlike `pending_attachment` - this is a map. Each entry
    /// holds the WebKit `URISchemeRequest` that must be finished with the
    /// fetched bytes (`AccountEvent::PartFetched`) or an error
    /// (`PartFetchFailed`, a timeout, or the user moving on). See
    /// `PendingCid`.
    pending_cid: HashMap<String, PendingCid>,
    /// The `cid:`-bearing parts of the message currently on the reading pane
    /// (a subset of the last rendered `EmailBody::parts`), used to resolve
    /// the `cid:` references WebKit's scheme handler forwards. Stale the
    /// moment the user navigates away; `render_body` re-stashes it.
    rendered_inline_parts: Vec<BodyPart>,
    /// Temporary files written for the row's "Open" action - attachments
    /// materialized on disk so the system's default handler can open them.
    /// Deleted when Lookout exits (`app.connect_shutdown` in `build_window`),
    /// so a viewer process is never left holding a file that's already gone.
    temp_attachment_files: HashSet<PathBuf>,
    /// The window's toast overlay, kept so the attachment Save flow (which
    /// runs from widget callbacks, not the account event loop) can surface
    /// fetch-timeout feedback. `None` only in tests, where no window exists.
    toast_overlay: Option<adw::ToastOverlay>,
    /// Persistent "Sending: <subject>" toasts shown while a `SendMessage`
    /// command is outstanding, queued per account in the order their sends
    /// were dispatched. The per-account session loop processes commands
    /// strictly in order, so `SendCompleted`/`SendFailed` always answers the
    /// oldest outstanding send for that account - popping the front on
    /// either event and dismissing it is enough to retract the right toast.
    sending_toasts: HashMap<AccountId, VecDeque<adw::Toast>>,
    /// A body is currently loading into the reading pane's WebView and
    /// should be revealed when its load finishes. Cleared on every selection
    /// change so a load started for a message the user has already navigated
    /// away from can never pop the pane back open (the persistent
    /// `load-changed` handler in the reading-pane build consults this before
    /// revealing).
    pending_html_reveal: bool,
    /// The summary of the message whose body is next to be revealed. The
    /// reading-pane header is updated from this only when the message page
    /// actually renders (`render_body`), never when the selection changes -
    /// so the previous message's header stays on screen for the whole
    /// fade-out instead of being swapped to the next email mid-fade.
    /// Cleared/overwritten on every selection change.
    pending_header: Option<EmailSummary>,
    /// Recently-viewed message bodies, so switching back to a message already
    /// read this session - or quoting it via Reply/Reply-All/Forward - does
    /// not re-fetch it from the server. Bounded LRU; see `BodyCache`. The
    /// worker-side `bodies` table in `lookout-mail`'s cache complements this
    /// with a disk layer that survives restarts.
    body_cache: BodyCache,
    /// Bumped on every selection change (alongside disarming
    /// `pending_html_reveal`). The HTML reveal-fallback timeout in
    /// `render_body` captures the value it was armed under and only reveals
    /// if it's unchanged, so a stale timeout armed for a message the user has
    /// already moved on from can never pop the *next* message's page open
    /// mid-load.
    reveal_generation: u64,
    /// The folder pane's last-selected view, loaded from disk at startup and
    /// rewritten whenever the selection changes (`select_mailbox` /
    /// `enter_unified_inbox`), so the pane reopens where the user left off -
    /// see `last_view`. `None` on the very first run, when the pane defaults
    /// to the "All Inboxes" unified view.
    last_selection: Option<LastSelection>,
    /// True while a remembered selection still has to be restored on startup.
    /// The restore is retried on every `FoldersUpdated` until the remembered
    /// mailbox's account has connected (see `restore_or_default_initial_view`),
    /// so a slow account can't cause its folder to be skipped; the default
    /// "All Inboxes" fallback only applies once restore is done or abandoned.
    /// Stops being relevant the moment the user clicks anything.
    restore_pending: bool,
    /// The `(mailbox, uid)` of the message whose body is currently displayed
    /// on the reading pane's "message" page. Lets the message-selection
    /// handler tell a re-selection of the already-open email apart from a
    /// real navigation, so a list rebuild that keeps the same row selected
    /// doesn't route the pane through "empty" and crossfade the same email
    /// again. `None` while the pane shows nothing.
    rendered_message: Option<(MailboxId, Uid)>,
    /// The parsed List-Unsubscribe actions of the message currently on the
    /// reading pane, re-derived by `render_body` from the rendered body's
    /// headers and read by the banner's button handler when it fires. `None`
    /// when the pane shows nothing or the message offers no unsubscribe
    /// action.
    unsubscribe_info: Option<lookout_core::ListUnsubscribe>,
    /// The `(mailbox, uid)` of the message whose unsubscribe banner the user
    /// acted on (clicked "Unsubscribe" - the banner's only affordance;
    /// Adw.Banner has no close button); while that message stays on screen
    /// the banner must not come back. Cleared on every navigation, so
    /// returning to the message later shows the banner again.
    unsubscribe_dismissed: Option<(MailboxId, Uid)>,
    /// The parsed iMIP invitation (or cancellation / RSVP reply) carried by
    /// the message currently on the reading pane's `text/calendar` part,
    /// re-derived by `render_body` and read by the banner's button handler
    /// when it fires. `None` when the pane shows nothing or the message
    /// carries no iMIP payload.
    imip: Option<lookout_core::ImipInvitation>,
    /// The `(mailbox, uid)` of the message whose iMIP banner the user acted
    /// on (chose a response, removed the event, or dismissed the notice);
    /// while that message stays on screen the banner must not come back.
    /// Cleared on every navigation, so returning to the message later shows
    /// the banner again.
    imip_dismissed: Option<(MailboxId, Uid)>,
    /// The addresses a read receipt for the message currently on the reading
    /// pane should go to - parsed from its `Disposition-Notification-To`
    /// header by `render_body`, read by the banner's button handler and the
    /// automatic-send path. `None` when the pane shows nothing or the
    /// message doesn't request a receipt.
    read_receipt_request: Option<Vec<String>>,
    /// The `(mailbox, uid)` of the message whose read-receipt banner the user
    /// acted on (sent the receipt); while that message stays on screen the
    /// banner must not come back. Cleared on every navigation, like
    /// `unsubscribe_dismissed`.
    read_receipt_dismissed: Option<(MailboxId, Uid)>,
    /// Every `(mailbox, uid)` a read receipt has been sent for this session -
    /// the fire-once guard for the automatic policy (a message re-opened
    /// later, or re-rendered by a Config toggle, must not receipt twice).
    /// Session-only, like the `load_once_images` override.
    read_receipts_sent: HashSet<(MailboxId, Uid)>,
    /// The original-message details a read receipt for the message currently
    /// on the reading pane needs, stashed by `render_body` (which has the
    /// body) alongside `read_receipt_request` and read by the send path. The
    /// `imip`/`unsubscribe_info` equivalent - per-render data, so the
    /// banner's button handler doesn't need the (LRU-evictable) body cache.
    read_receipt_context: Option<ReadReceiptContext>,
    /// Mailboxes with a `SyncMailbox` request outstanding - sent but not yet
    /// answered by a `MessagesUpdated`. The startup burst (the session's
    /// cache replay plus the app's on-demand syncs) would otherwise queue
    /// several identical inbox syncs; dedupe them in `request_mailbox_sync`.
    /// Entries are cleared when the mailbox delivers, or when its account
    /// reconnects and sends fresh folders, so a request that dies with a
    /// dropped connection can't suppress a later one.
    syncing: HashSet<MailboxId>,
    /// How the message list is ordered, set from the list header's sort
    /// controls. Applied in `repopulate_message_list` - the single choke point
    /// every list rebuild passes through - so the order is uniform no matter
    /// which event produced the rebuild.
    sort_key: SortKey,
    /// True for newest/Z-A first, the order the list was hardcoded to before
    /// the sort controls existed.
    sort_descending: bool,
    /// Mailboxes the user has starred in the message-list header, rendered as
    /// a "Favorites" section pinned to the top of the folder tree. Persisted
    /// via the `mail-favorites` GSettings key, loaded at startup and written
    /// through on every star toggle (see `settings`).
    favorites: HashSet<MailboxId>,
    /// Config → Mail → "Load images from the web": whether the reading pane's
    /// WebView may load remote `image/*` subresources. Consulted by the
    /// load-policy handler on every resource decision. Persisted via the
    /// `mail-load-remote-images` GSettings key.
    load_remote_images: bool,
    /// Config → Mail → "Rich text": the default body mode for new compose
    /// sessions, read when the composer opens. Persisted via the
    /// `mail-rich-text-default` GSettings key.
    rich_text_default: bool,
    /// Trusted-sender entries (`name@example.com` or `@example.com`,
    /// normalized lowercase) with their trust level, keyed by the receiving
    /// account - the persisted shape of the external-content trust flow. The
    /// reading pane's load-policy handler consults this for the message
    /// currently on screen. Persisted in the UI-state database
    /// (`ui_state_db`), loaded at startup and written through on every
    /// trust/revoke action, like `starred_contacts`.
    pub(crate) trusted_senders: HashMap<(AccountId, String), lookout_core::TrustLevel>,
    /// The `(account, normalized sender address)` of the message currently on
    /// the reading pane, re-stashed by `render_body` from the rendered
    /// summary's From address. The load-policy handler resolves this against
    /// `trusted_senders` on every resource decision; `None` when the pane
    /// shows nothing or the message has no usable sender (the debug `.eml`
    /// viewer, which also has no account to key trust on).
    rendered_trust_sender: Option<(AccountId, String)>,
    /// Session-only "load remote images just this once" override for the
    /// message currently on the reading pane - the external-content banner's
    /// transient action. Cleared on every navigation.
    load_once_images: bool,
    /// The header's "Switch message theme" toggle for the message currently
    /// on the reading pane: when true, the message body's own background
    /// colour is stripped and its colours inverted (see
    /// `MESSAGE_THEME_OVERRIDE_CSS`) so the app theme's background shows
    /// through with the content still readable against it. A per-email
    /// override - reset to the Config → Appearance "Dark message theme"
    /// default on every navigation (alongside the other per-message state,
    /// and always in lockstep with the physical `set_message_theme_armed`),
    /// and `render_body` syncs the header button from it so the next message
    /// opens in the configured default.
    message_theme_override: bool,
    /// The `(mailbox, uid)` of the message whose external-content banner the
    /// user acted on (trusted the sender or loaded once); while that message
    /// stays on screen the banner must not come back. Cleared on every
    /// navigation, like `unsubscribe_dismissed`.
    trust_banner_dismissed: Option<(MailboxId, Uid)>,
    /// The last `html_remote_content_scan` result, keyed by the message it
    /// was computed for. The scan re-parses the rendered message's whole HTML
    /// (`window.rs`'s trust banner), which is unchanged across re-renders of
    /// the same message, so the banner block reuses the cached scan instead
    /// of rescanning on every render. Only the current message is retained -
    /// a single-slot cache, sized to what `render_body` actually re-visits.
    rendered_remote_scan: Option<(MailboxId, Uid, lookout_core::RemoteContentScan)>,
    /// Relay to the currently-open composer for its draft-autosave
    /// confirmations: the account event loops forward `DraftSaved`
    /// Message-Ids here, and the composer flips its "Saving draft…" label to
    /// "Draft saved" when its own id arrives. `None` while no composer is
    /// open; replaced whenever a new composer opens (dropping the previous
    /// sender lets the old composer's consumer exit).
    draft_saved_tx: Option<async_channel::Sender<String>>,
    /// Refresh hook for the currently-open composer's From dropdown: the
    /// Config → Mail accounts manage-identities dialog fires it after every
    /// change, so an identity added/edited while a composer is open shows up
    /// in its From list immediately. `None` while no composer is open.
    composer_identities_refresh: Option<Rc<dyn Fn()>>,
    /// Which compose session's relays (`draft_saved_tx` and
    /// `composer_identities_refresh`) are currently installed. Every composer
    /// bumps this when it opens and remembers its own value; a finishing
    /// composer clears the relays only if its generation still owns them, so
    /// a popped-out composer that finishes after a newer inline composer
    /// opened can't strip the newer composer's relays.
    composer_relay_generation: u64,
    /// The pop-out window hosting the composer, while one exists (the
    /// composer header's pop-out button moves the still-alive composer into
    /// its own window; `None` otherwise), paired with the composer
    /// generation that created it (its captured `composer_relay_generation`
    /// value) - so a finishing composer closes only its own window, never a
    /// newer composer's. Kept here so the window survives after the button
    /// handler returns - GTK only keeps a presented window alive for as long
    /// as it's on screen. The window's close handler pops the composer back
    /// into the reading pane, and a finishing composer destroys the window
    /// outright; both clear this.
    compose_popout_window: Option<(u64, adw::Window)>,
    /// What the folder sidebar currently has rendered (see
    /// `folder_tree_signature`). `FoldersUpdated` now arrives repeatedly per
    /// account as the unread counts fill in, and almost all of those carry
    /// nothing the tree draws; comparing against this skips the rebuild
    /// entirely for them. `None` until the first tree is built.
    folder_tree: Option<FolderTreeSignature>,
    /// Set while `rebuild_folder_tree` is putting the selection back after
    /// swapping the model, so the `selected-item` handler doesn't mistake the
    /// restore for the user clicking a folder. Without it, re-selecting the
    /// open mailbox would re-issue its sync, and the momentary landing on row
    /// 0 that `GtkSingleSelection`'s autoselect does on every `set_model`
    /// would enter the unified view.
    suppress_folder_selection: bool,
    /// Whether the message list is showing full-text search results. True only
    /// with a non-empty `search_query`; `MailView::Search` (see
    /// `MailView`) is the list's view mode while active. Exiting restores the
    /// pre-search view: `Single` when `current_mailbox` survived the search,
    /// else the unified "All Inboxes" view.
    search_active: bool,
    /// The query being searched for, matching the search entry's text. The
    /// list shows results as `(mailbox, uid)` sets that match it.
    search_query: String,
    /// The accumulated search results - the instant FTS cache pass over every
    /// account merged with each live IMAP `SEARCH` answer as it arrives,
    /// deduplicated by `(mailbox, uid)`. Repopulated into the message list on
    /// every change.
    search_results: Vec<EmailSummary>,
    /// The `(account, mailbox)` pairs the live IMAP pass has asked for and not
    /// yet been answered. `SearchResults` fires once per requested folder
    /// (always, even for an empty match set - see the session docs), removing
    /// its entry; an empty set means the live pass is done.
    search_pending: HashSet<(AccountId, MailboxId)>,
}

impl UiState {
    /// Whether a GOA account is enabled: everything is enabled unless its id
    /// sits in the `accounts-disabled` preference (Config → Accounts).
    /// `pub(crate)` for `contacts_view`'s discovery filtering.
    pub(crate) fn account_enabled(&self, id: &AccountId) -> bool {
        !self.settings.get_strv(crate::settings::ACCOUNTS_DISABLED).iter().any(|disabled| disabled == &id.0)
    }

    /// Marks a GOA account enabled/disabled, persisting the whole disabled
    /// set through the `accounts-disabled` preference.
    pub fn set_account_enabled(&self, id: &AccountId, enabled: bool) {
        let mut disabled = self.settings.get_strv(crate::settings::ACCOUNTS_DISABLED);
        disabled.retain(|existing| existing != &id.0);
        if !enabled {
            disabled.push(id.0.clone());
        }
        self.settings.set_strv(crate::settings::ACCOUNTS_DISABLED, disabled);
    }
}

/// Per-calendar-account state, kept separate from `UiState`/`AccountHandle`
/// (Mail's equivalents) matching the crate's existing per-domain-type
/// separation - Calendar is a wholly independent account set from Mail.
struct CalendarAccountHandle {
    cmd_tx: async_channel::Sender<CalendarCommand>,
    display_name: String,
    /// The account's CalDAV base URL, kept for the Config view's account
    /// overview.
    uri: String,
    calendars: Vec<CalendarInfo>,
    /// Latest reported session state, rendered in the sidebar's "My
    /// calendars" checklist while the account hasn't delivered any
    /// calendars yet (see `refresh_calendar_checklist`).
    connection_state: CalConnectionState,
    /// Latest occurrences for whatever month this account last synced,
    /// keyed by month so a stale resync from one account can't clobber
    /// another account's occurrences for the currently-displayed month -
    /// same "only apply if it matches what's on screen" principle as Mail's
    /// `MessagesUpdated` handling.
    last_occurrences: Vec<EventOccurrence>,
    last_synced_month: Option<chrono::NaiveDate>,
    /// Occurrences keyed by the month they were synced in, pruned to the
    /// current and next month. The Lookout dashboard's "upcoming events"
    /// section reads this union: a single month's window would drain as
    /// events pass and the session's poll never advances past its one
    /// polled month. The calendar view and reminders keep using
    /// `last_occurrences`, so this map stays dashboard-only.
    occurrences_by_month: HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
    /// Latest full task list from the account's last `TasksUpdated` - tasks
    /// have no month window, so a whole-set snapshot is the natural unit.
    last_tasks: Vec<CalendarTask>,
}

/// The original-message details a read receipt (RFC 8098 MDN) is built
/// from, stashed by `render_body` alongside the parsed
/// `Disposition-Notification-To` request. The builder (`lookout_mail`'s
/// `ReadReceipt`) needs the original's Message-ID/From/Subject/Date and its
/// headers (the report's third part); keeping them here means the banner's
/// button handler - and the automatic policy - never touch the
/// LRU-evictable body cache.
#[derive(Clone)]
struct ReadReceiptContext {
    message_id: String,
    original_from: String,
    subject: String,
    date: Option<String>,
    headers: Vec<(String, String)>,
}

/// Per-subscription state for one webcal feed - the fetch-only cousin of
/// `CalendarAccountHandle`. One handle per configured subscription, aligned
/// with `CalendarUiState::webcal_subscriptions` by subscription id; events
/// carry the synthetic calendar id `"webcal:<id>"` so the shared
/// calendar-id machinery (checklist toggles, checked set, colors, view
/// merge) works unchanged.
struct WebcalHandle {
    /// The synthetic calendar id its events carry (`"webcal:<id>"`) - how
    /// the handle's occurrences are filtered/colored/keyed everywhere.
    calendar_id: CalendarId,
    display_name: String,
    /// Latest occurrences for whatever month last synced - keyed the same
    /// "only apply if it matches what's on screen" way as
    /// `CalendarAccountHandle::last_occurrences`.
    last_occurrences: Vec<EventOccurrence>,
    last_synced_month: Option<chrono::NaiveDate>,
    /// Per-month occurrences for the Lookout dashboard, pruned like
    /// `CalendarAccountHandle::occurrences_by_month`.
    occurrences_by_month: HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
    /// The feed's latest fetch error, if any - the UI toasts on the
    /// transition into error, not on every 5-minute poll.
    error: Option<String>,
}

/// State for the synthesized "Birthdays" calendar - the calendar-id-keyed
/// cousin of `WebcalHandle`, but there is no fetch or poll to run: the
/// source is the `UiState::contacts_by_account` snapshots the CardDAV sync
/// already maintains, copied in on every contacts update, and the
/// per-month occurrences are recomputed in place (cheap: a few thousand
/// contacts at most) instead of cached. `None` until any contacts exist, so
/// the checklist's "Birthdays" row only appears when it has a data source.
struct BirthdaysHandle {
    /// The synthetic calendar id (`"birthdays"`) its events carry - the
    /// shared calendar-id machinery works unchanged, and read-only parity
    /// with webcal feeds falls out of the same id-based checks.
    calendar_id: CalendarId,
    display_name: String,
    /// The source contacts, one batch per account, replaced whenever a
    /// contacts snapshot lands (the signal that `BDAY` data changed).
    contacts: Vec<(AccountId, Vec<lookout_dav::ContactRecord>)>,
    /// Latest occurrences for whatever month last synced, computed by
    /// `sync_month` - keyed the same "only apply if it matches what's on
    /// screen" way as `CalendarAccountHandle::last_occurrences`.
    last_occurrences: Vec<EventOccurrence>,
    last_synced_month: Option<chrono::NaiveDate>,
    /// Per-month occurrences for the Lookout dashboard, computed by
    /// `sync_dashboard_window` and pruned like
    /// `CalendarAccountHandle::occurrences_by_month`.
    occurrences_by_month: HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
}

impl BirthdaysHandle {
    /// Replaces the source contact set (called when a contacts snapshot
    /// lands) - the only thing that can change birthday data besides the
    /// passage of time.
    fn set_contacts(&mut self, contacts: Vec<(AccountId, Vec<lookout_dav::ContactRecord>)>) {
        self.contacts = contacts;
    }

    /// (Re)computes the handle's occurrences for `month` - the scoped
    /// snapshot the calendar views, print path, and mini-calendar read.
    /// Deterministic (sorted by start then uid) so repeated calls are
    /// diff-friendly.
    fn sync_month(&mut self, month: chrono::NaiveDate) {
        self.last_occurrences = birthday_occurrences_batch(&self.contacts, &self.calendar_id, month);
        self.last_synced_month = Some(month);
    }

    /// (Re)computes the dashboard-horizon months (current + next) - the
    /// same window the account sessions' `FetchMonth` commands cover, so
    /// the dashboard's upcoming-events section and the reminder engine see
    /// next month's birthdays too.
    fn sync_dashboard_window(&mut self) {
        for month in dashboard_month_window() {
            let occurrences = birthday_occurrences_batch(&self.contacts, &self.calendar_id, month);
            insert_dashboard_occurrences(&mut self.occurrences_by_month, month, occurrences);
        }
    }
}

/// The synthetic calendar id every birthday occurrence carries.
fn birthdays_calendar_id() -> CalendarId {
    CalendarId("birthdays".to_string())
}

/// Runs `lookout_dav::birthday_occurrences` across every account's contact
/// batch and merges the results into one sorted set - the batch-level mirror
/// of the per-account function, kept here since `ContactRecord`'s account
/// grouping is an app-side concern.
fn birthday_occurrences_batch(contacts: &[(AccountId, Vec<lookout_dav::ContactRecord>)], calendar_id: &CalendarId, month: chrono::NaiveDate) -> Vec<EventOccurrence> {
    let mut occurrences: Vec<EventOccurrence> = contacts
        .iter()
        .flat_map(|(account_id, contacts)| lookout_dav::birthday_occurrences(account_id, calendar_id, contacts, month))
        .collect();
    occurrences.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.0.cmp(&b.uid.0)));
    occurrences
}

/// Per-Google-account state for the Google Tasks integration (keyed by the
/// account's email). One `run_google_tasks_session` actor per connected
/// account; its task lists map to synthetic `googletasks:<list id>`
/// calendar ids so the Tasks view's colour/merge machinery treats them like
/// calendars.
struct GoogleTasksHandle {
    cmd_tx: async_channel::Sender<GoogleTasksCommand>,
    email: String,
    /// The account's task lists, from the last `ListsUpdated` - the task
    /// editor's picker entries and the save-routing lookup.
    task_lists: Vec<TaskList>,
    /// Latest full task snapshot from the account's last `TasksUpdated`.
    last_tasks: Vec<CalendarTask>,
    /// The latest reported error, if any (revoked token, failed write, ...).
    error: Option<String>,
}

struct CalendarUiState {
    accounts: HashMap<AccountId, CalendarAccountHandle>,
    displayed_month: chrono::NaiveDate,
    /// Which calendars (by id) are currently shown - unioned in
    /// `refresh_displayed_calendar_view`, toggled from the sidebar's "My
    /// calendars" checklist. Newly-discovered calendars default to checked
    /// (shown) - see `refresh_calendar_checklist`.
    checked_calendar_ids: HashSet<CalendarId>,
    /// Each calendar's assigned colour, persisted between sessions (see
    /// `calendar_colors`). Kept alongside `checked_calendar_ids` so the
    /// checklist can colour its checkboxes; new calendars are assigned here in
    /// `refresh_calendar_checklist`.
    calendar_colors: calendar_colors::CalendarColorMap,
    /// Command channel to the single webcal feed session (all subscriptions
    /// are polled by one actor), or `None` before `spawn_webcal_session` runs.
    /// Subscriptions are added/removed by sending the session the full new
    /// list via `SubscriptionCommand::Reload`.
    webcal_cmd_tx: Option<async_channel::Sender<SubscriptionCommand>>,
    /// Working copy of `AppConfig::webcal_subscriptions`, kept here so the
    /// checklist (and the session) can be driven without reaching into the
    /// config store on every event. Mutated only by the add/manage dialog,
    /// which persists the authoritative list back to `settings.json`.
    webcal_subscriptions: Vec<WebcalSubscription>,
    /// Per-subscription feed state, keyed by subscription id.
    webcal_handles: HashMap<String, WebcalHandle>,
    /// The synthesized "Birthdays" calendar, `None` until any contacts exist
    /// (the checklist row appears only when it has a data source - mirroring
    /// how the webcal group appears only when subscriptions exist).
    birthdays: Option<BirthdaysHandle>,
    /// Connected Google Tasks accounts, keyed by email.
    google_tasks: HashMap<String, GoogleTasksHandle>,
    /// Every Google GOA account's email, discovered at startup - the
    /// "Connect Google Tasks" toolbar button's targets.
    google_account_emails: Vec<String>,
    /// Locally-stored tasks (`CalendarId("local")`) - the fallback store
    /// used when no connected source supports tasks. Survives "Clear all
    /// caches" (it lives in the UI-state database, not a cache).
    local_tasks: Vec<CalendarTask>,
    /// Best-effort handle on the UI-state database for local-task writes;
    /// `None` when it couldn't open (local tasks then live in memory only).
    local_tasks_db: Option<Rc<RefCell<UiStateDb>>>,
    /// The Lookout dashboard's repaint hook, registered by the window once
    /// `calendar_state` exists. `refresh_tasks_view` and the calendar event
    /// loops call it so the dashboard stays live; `None` until then (a
    /// no-op, never an error).
    dashboard_refresh: Option<Rc<dyn Fn()>>,
    /// The mail toolbar's "Add as Task" flag button's own repaint hook,
    /// registered by the window once `calendar_state` exists - same pattern
    /// as `dashboard_refresh`. `refresh_tasks_view` calls it so the button's
    /// filled/outline icon stays in sync with whichever message is selected
    /// as tasks are created, synced, or removed; `None` until then.
    task_button_refresh: Option<Rc<dyn Fn()>>,
    /// Occurrences with a drag-reschedule in flight, keyed by `(uid,
    /// recurrence_id)`, holding the occurrence with its new `start`/`end`
    /// already applied. Reapplied onto every `refresh_displayed_calendar_view`
    /// repaint until an incoming occurrence's own start/end already matches -
    /// meaning the server has confirmed it - at which point the entry is
    /// dropped. Rolled back (removed, no reapply) on `EventSaveFailed`.
    pending_calendar_moves: HashMap<(EventUid, Option<chrono::DateTime<chrono::Utc>>), EventOccurrence>,
    /// The Mail-screen overview pane's task rows' click-to-edit handler,
    /// registered by the window once `calendar_state` exists -
    /// `refresh_mail_overview_day_list` reads it to build rows that open the
    /// shared task editor (the overview's rows carry no completion checkbox).
    mail_overview_activate: Option<crate::tasks_view::ActivateHandler>,
    /// The Mail-screen overview pane's repaint hook, registered by the window
    /// once `calendar_state` exists - same pattern as `dashboard_refresh`.
    /// `refresh_tasks_view` calls it so the pane's task rows stay live as
    /// tasks are created, synced, toggled, or removed; `None` until then.
    mail_overview_refresh: Option<Rc<dyn Fn()>>,
}

/// Strips `Gtk.Paned`'s default visible grey separator line - the card
/// margins already provide a visual gap between panes (see `card_section`),
/// so a painted handle on top of that just looks like a stray line. The
/// handle keeps a comfortable draggable hit-area (`min-width`/`min-height`);
/// only its painted background/border is removed.
///
/// Also gives the folder card (`.folder-pane`, see `card_section`'s caller)
/// a 50%-alpha black background instead of libadwaita's normal opaque
/// `.card` fill, so the window background image shows through it.
fn install_paned_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "paned > separator {
            background: none;
            border: none;
            box-shadow: none;
            min-width: 12px;
            min-height: 12px;
        }
        .folder-pane {
            background-color: @lookout-pane-bg;
        }
        /* The black widget overlaid on the window background image while the
           user dims it (Config → Appearance → 'Background dimming'): its
           opacity, set from the widget, controls how much the image darkens
           toward black. */
        .window-background-dim {
            background-color: @lookout-dim;
        }
        /* The folder rows' trailing unread count. Bold and accent-blue to
           match the message list's unread rows. Tabular figures ('tnum') so
           the digits are the same width in every row - without them the
           counts visibly jitter as they update. Single-quoted because this
           whole stylesheet is a plain Rust string literal. */
        .folder-unread-count {
            color: @lookout-unread;
            font-weight: bold;
            font-size: 0.9em;
            font-feature-settings: 'tnum';
            padding-right: 6px;
        }
        .folder-pane listview,
        .folder-pane list,
        .folder-pane scrolledwindow {
            background-color: transparent;
        }
        /* A folder or tag row accepting a message drag: a rounded accent
           outline marks the drop target while the drag hovers over it. */
        .lookout-drop-target {
            outline: 2px solid alpha(currentColor, 0.45);
            outline-offset: -2px;
            border-radius: 8px;
        }
        .reading-pane-transparent {
            background-color: transparent;
        }
        .card-flush-end {
            border-top-right-radius: 0;
            border-bottom-right-radius: 0;
        }
        .card-flush-start {
            border-top-left-radius: 0;
            border-bottom-left-radius: 0;
        }
        .seamless-paned > separator {
            min-width: 6px;
            min-height: 6px;
        }
        .window-toolbars-background {
            background-color: @lookout-toolbar-band;
            border-radius: 8px;
        }
        .window-icon-toolbar-background {
            background-color: @lookout-icon-toolbar-bg;
            border-radius: 8px;
        }
        .message-header-subject {
            font-weight: bold;
            font-size: 1.05em;
        }
        .message-subject-bar {
            background-color: @lookout-subject-bar-bg;
            border-bottom: 1px solid @lookout-subject-bar-border;
            padding: 10px 12px;
        }
        .message-action-bar {
            padding: 10px 12px;
        }
        .message-header-meta {
            opacity: 0.7;
        }
        .avatar-circle {
            border-radius: 9999px;
            color: white;
            font-weight: bold;
        }
        .avatar-color-0 { background-color: @lookout-avatar-0; }
        .avatar-color-1 { background-color: @lookout-avatar-1; }
        .avatar-color-2 { background-color: @lookout-avatar-2; }
        .avatar-color-3 { background-color: @lookout-avatar-3; }
        .avatar-color-4 { background-color: @lookout-avatar-4; }
        .avatar-color-5 { background-color: @lookout-avatar-5; }
        .hover-quick-actions {
            background-color: @theme_bg_color;
            border-radius: 8px;
            padding: 2px;
            box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
        }
        button.hover-quick-action {
            background-color: @theme_bg_color;
            border-radius: 6px;
            padding: 1px;
            min-width: 22px;
            min-height: 22px;
            box-shadow: none;
        }
        button.hover-quick-action:hover {
            background-color: @theme_selected_bg_color;
            color: @theme_selected_fg_color;
        }
        .ribbon-tab {
            border-radius: 8px;
            margin: 2px 0;
        }
        .ribbon-tab:checked {
            background-color: @lookout-icon-toolbar-bg;
        }
        .ribbon-group-label {
            margin-right: 6px;
        }
        button.list-header-action {
            min-width: 24px;
            min-height: 24px;
            padding: 2px;
        }
        /* Zeroing the row's own padding is what lets the unread accent bar
           sit flush against the pane's leading edge, and flattens the
           selection highlight's inset to a full-bleed band. */
        .message-list > row {
            padding: 0;
        }
        .message-row {
            border-bottom: 1px solid @lookout-row-separator;
        }
        .attachment-row {
            padding: 4px 6px;
            border-radius: 6px;
        }
        .attachment-row:hover {
            background-color: @theme_selected_bg_color;
            color: @theme_selected_fg_color;
        }
        .message-accent-bar {
            background-color: transparent;
        }
        .message-accent-bar.unread {
            background-color: @lookout-unread;
        }
        /* Amber rather than the list's blue: the flag has to read as a
           separate axis from unread, which owns every blue accent here. */
        .message-flag-icon {
            color: @lookout-flag;
        }
        /* The attachment and meeting-invite indicators, dimmed so they stay
           secondary to the sender/subject text and the amber flag. */
        .message-row-icon {
            color: alpha(currentColor, 0.5);
        }
        /* Color-tag dots. The circle shape is what makes one tag read as a
           swatch at a glance; each tag's fill comes from the per-tag
           `.message-tag-dot.tag-<key>` rules `apply_tag_colors` maintains,
           not from this base rule. */
        .message-tag-dot {
            border-radius: 9999px;
            background-color: transparent;
        }
        /* Recipient chips. The pill shape is what separates one recipient
           from the next at a glance - the whole point of chips over a run of
           comma-separated text. */
        .recipient-field {
            padding: 6px 10px;
        }
        .recipient-chip {
            background-color: @lookout-chip-bg;
            border: 1px solid @lookout-chip-border;
            border-radius: 999px;
            padding: 1px 2px 1px 10px;
        }
        /* A chip that doesn't parse as an address is flagged, never
           rejected - the user has to be able to see and fix it. */
        .recipient-chip.recipient-chip-invalid {
            background-color: @lookout-chip-invalid-bg;
            border-color: @lookout-chip-invalid-border;
        }
        .recipient-chip-remove {
            min-width: 18px;
            min-height: 18px;
            padding: 0;
        }
        .message-sender-unread,
        .message-subject-unread,
        .message-date-unread {
            color: @lookout-unread;
            font-weight: bold;
        }
        .message-sender-read,
        .message-date-read {
            color: @lookout-muted;
        }
        .message-subject-read {
            color: @lookout-subject;
        }
        .message-preview {
            color: @lookout-muted;
            opacity: 0.75;
            font-size: 0.95em;
        }
        .message-section-header {
            background-color: @lookout-section-header-bg;
            border-top: 1px solid @lookout-row-separator;
            border-bottom: 1px solid @lookout-row-separator;
        }
        /* Conversation headers read as a distinct row kind from both section
           headers (darker band) and messages (bordered): a subtle tint that
           also backs the expander's chevron so the thread's disclosure is
           visible against the background image. */
        .message-thread-row {
            background-color: @lookout-thread-row-bg;
            border-bottom: 1px solid @lookout-row-separator;
        }
        /* The participant-count badge: a pill so the number reads as a count,
           not a fourth text column. */
        .message-thread-count {
            font-size: 0.9em;
            background-color: @lookout-thread-count-bg;
            border-radius: 9999px;
            padding: 0 7px;
        }
        .message-column-header {
            background-color: @lookout-column-header-bg;
            border-top: 1px solid @lookout-row-separator;
            border-bottom: 1px solid @lookout-row-separator;
            padding: 4px 0;
        }
        .message-column-header label {
            color: @lookout-column-header-fg;
            font-size: 0.85em;
            font-weight: bold;
        }
        entry.header-search-entry {
            background-color: @lookout-header-search-bg;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub fn build_window(app: &adw::Application, worker: Rc<Worker>) -> adw::ApplicationWindow {
    // The theme stack (base palette + bundled flat-token theme + custom
    // accent) registers before any other CSS provider so the app's rules can
    // reference its `lookout-*` tokens. Seeded from GSettings when the Config
    // rows are wired further down.
    let theme_manager = crate::theme::ThemeManager::install();
    install_paned_css();

    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| app.quit()
    });
    app.add_action(&quit_action);

    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(|_, _| {
        adw::AboutDialog::builder()
            .application_name("Lookout")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Gavin Graham")
            .license_type(gtk::License::Gpl30)
            .comments("A native GNOME mail client for GNOME Online Accounts.")
            .build()
            .present(gtk::Window::NONE);
    });
    app.add_action(&about_action);

    // Phase 5: the process-wide preference store, resolved once up front
    // (GSettings-backed when the schema is available, session-only memory
    // otherwise) and handed everywhere preferences are read or written -
    // including `background_image`/`last_view`, which need it before
    // `UiState` exists. One-time imports of the pre-GSettings plain files
    // run before the first reads below.
    let settings = Rc::new(crate::settings::resolve());
    crate::last_view::migrate_legacy(&settings);
    crate::background_image::migrate_legacy(&settings);

    // --- Background portal approval: ask the session's portal once (the
    // shell shows a dialog the first time, then remembers) so Lookout is
    // listed among the desktop's background apps and can be stopped there,
    // whenever the close-to-background feature is on. Fire-and-forget: no
    // portal (or a denial) only means the app is invisible to that listing,
    // not that background running stops.
    if settings.get_bool(crate::settings::CLOSE_TO_BACKGROUND) {
        worker.spawn(async { crate::background::request_background_approval().await });
    }

    let bg_bytes = crate::resources::bytes("/io/github/gavindi/Lookout/backgrounds/background2.jpg")
        .unwrap_or_else(|| glib::Bytes::from_static(include_bytes!("../../../data/resources/backgrounds/background2.jpg")));
    let default_bg_texture = gtk::gdk::Texture::from_bytes(&bg_bytes).expect("bundled background image should decode");
    let background = gtk::Picture::for_paintable(&default_bg_texture);
    background.set_content_fit(gtk::ContentFit::Cover);
    background.set_can_shrink(true);
    background.set_hexpand(true);
    background.set_vexpand(true);
    // A custom background chosen under Config → Appearance → "Window
    // background" wins over the bundled artwork when it's still around and
    // still decodes; the Config view rows are told about it further down.
    let custom_background_name = crate::background_image::load(&settings).and_then(|path| match gtk::gdk::Texture::from_filename(&path) {
        Ok(texture) => {
            background.set_paintable(Some(&texture));
            path.file_name().map(|name| name.to_string_lossy().into_owned())
        }
        Err(_) => {
            tracing::warn!(path = %path.display(), "ignoring unreadable custom background image");
            None
        }
    });

    let toast_overlay = adw::ToastOverlay::new();

    let status_page = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("No Mail Accounts")
        .description("Add an account with Mail enabled in GNOME Online Accounts to get started.")
        .build();
    let open_settings_button = gtk::Button::builder()
        .label("Open Online Accounts Settings")
        .halign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    open_settings_button.connect_clicked({
        let worker = worker.clone();
        move |_| worker.spawn(crate::online_accounts::open_online_accounts())
    });
    status_page.set_child(Some(&open_settings_button));

    // --- Folder sidebar: one expanded-by-default group per account ---
    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_factory = gtk::SignalListItemFactory::new();
    // The folder rows are built before `state` exists (it's created further
    // down, once the mail accounts are wired up), so the drop target reaches
    // it through this slot, filled right after `state` is created.
    let state_slot: Rc<RefCell<Option<Rc<RefCell<UiState>>>>> = Rc::new(RefCell::new(None));
    {
        let state_slot = state_slot.clone();
        folder_factory.connect_setup(move |_, list_item| {
            let expander = gtk::TreeExpander::new();
            let icon = gtk::Image::builder().icon_size(gtk::IconSize::Normal).build();
            // `hexpand` on the name, not the box, is what pushes the count to the
            // trailing edge: the name takes all the slack (and ellipsizes into it
            // when the pane is narrow) and the count keeps its natural width.
            let label = gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            let count = gtk::Label::builder()
                .xalign(1.0)
                .margin_top(6)
                .margin_bottom(6)
                .css_classes(["folder-unread-count"])
                .build();
            let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
            row_box.append(&icon);
            row_box.append(&label);
            row_box.append(&count);
            expander.set_child(Some(&row_box));
            // Drop target: a folder row accepts the internal message-drag
            // payload and moves the messages into that folder. The target
            // identity (which folder this row shows) is written by `bind`
            // into `drop_data`, since the drop can't read the model itself.
            let drop_data: Rc<RefCell<Option<Mailbox>>> = Rc::new(RefCell::new(None));
            let drop_target = gtk::DropTarget::builder()
                .formats(&gtk::gdk::ContentFormats::for_type(glib::Bytes::static_type()))
                .actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE)
                .build();
            {
                let drop_data = drop_data.clone();
                let state_slot = state_slot.clone();
                drop_target.connect_drop(move |_target, value, _x, _y| {
                    let Some(mailbox) = drop_data.borrow().clone() else {
                        return false;
                    };
                    let Some(state) = state_slot.borrow().clone() else {
                        return false;
                    };
                    handle_message_drag_drop(&state, &mailbox, value)
                });
            }
            {
                let expander = expander.clone();
                drop_target.connect_enter(move |_target, _x, _y| {
                    expander.add_css_class("lookout-drop-target");
                    gtk::gdk::DragAction::MOVE
                });
            }
            {
                let expander = expander.clone();
                drop_target.connect_leave(move |_target| {
                    expander.remove_css_class("lookout-drop-target");
                });
            }
            expander.add_controller(drop_target);
            // Expunge quick action (initially hidden), mirroring the message
            // rows' hover quick actions: a button that appears on hover over
            // a Trash/Junk row and empties that folder. `bind` writes which
            // folder the button acts on and whether the row is eligible into
            // `expunge_data`/`expunge_enabled`, read at click time - the same
            // slot pattern as the drop target above.
            let expunge_btn = gtk::Button::from_icon_name("user-trash-full-symbolic");
            expunge_btn.add_css_class("hover-quick-action");
            expunge_btn.set_tooltip_text(Some("Empty folder"));
            let action_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(0)
                .halign(gtk::Align::End)
                .valign(gtk::Align::Center)
                .margin_end(8)
                .build();
            action_box.add_css_class("hover-quick-actions");
            action_box.append(&expunge_btn);
            action_box.set_visible(false);
            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&expander));
            overlay.add_overlay(&action_box);
            let expunge_data: Rc<RefCell<Option<Mailbox>>> = Rc::new(RefCell::new(None));
            let expunge_enabled: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            {
                let expunge_data = expunge_data.clone();
                let state_slot = state_slot.clone();
                let expunge_btn_for_dialog = expunge_btn.clone();
                expunge_btn.connect_clicked(move |_| {
                    let Some(mailbox) = expunge_data.borrow().clone() else {
                        return;
                    };
                    // Irreversible, so confirm first - unlike delete-to-Trash
                    // there's no undo for an expunge.
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Empty {}?", display_name(&mailbox.name)))
                        .body("All messages in this folder will be permanently deleted. This cannot be undone.")
                        .default_response("cancel")
                        .close_response("cancel")
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("empty", "Empty");
                    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
                    let expunge_data = expunge_data.clone();
                    let state_slot = state_slot.clone();
                    dialog.connect_response(None, move |_dialog, response| {
                        if response != "empty" {
                            return;
                        }
                        let Some(mailbox) = expunge_data.borrow().clone() else { return };
                        let Some(account_id) = mailbox_account_id(&mailbox.id) else { return };
                        let Some(state) = state_slot.borrow().clone() else { return };
                        let state = state.borrow();
                        if let Some(handle) = state.accounts.get(&account_id) {
                            let _ = handle.cmd_tx.send_blocking(AccountCommand::EmptyMailbox { mailbox: mailbox.id });
                        }
                    });
                    dialog.present(Some(&expunge_btn_for_dialog));
                });
            }
            {
                let action_box = action_box.clone();
                let expunge_enabled = expunge_enabled.clone();
                let hover_controller = gtk::EventControllerMotion::new();
                let action_box_for_leave = action_box.clone();
                hover_controller.connect_enter(move |_, _, _| {
                    // Only Trash/Junk rows carry an expunge action.
                    if expunge_enabled.get() {
                        action_box.set_visible(true);
                    }
                });
                hover_controller.connect_leave(move |_| {
                    action_box_for_leave.set_visible(false);
                });
                overlay.add_controller(hover_controller);
            }
            unsafe {
                expander.set_data(
                    "lookout-expunge-widgets",
                    FolderRowWidgets {
                        action_box,
                        expunge_btn,
                        expunge_data,
                        expunge_enabled,
                    },
                );
            }
            unsafe {
                expander.set_data("lookout-drop-target", drop_data);
            }
            list_item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&overlay));
        });
    }
    folder_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else { return };
        let Some(overlay) = list_item.child().and_downcast::<gtk::Overlay>() else {
            return;
        };
        let Some(expander) = overlay.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        expander.set_list_row(Some(&row));
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let tree_item = boxed.borrow::<TreeItem>();
        let Some(row_box) = expander.child().and_downcast::<gtk::Box>() else { return };
        let Some(icon) = row_box.first_child().and_downcast::<gtk::Image>() else { return };
        let Some(label) = icon.next_sibling().and_downcast::<gtk::Label>() else { return };
        let Some(count) = row_box.last_child().and_downcast::<gtk::Label>() else { return };
        // A zero count is hidden rather than blanked: an empty label still
        // reserves its spacing, which would leave every quiet folder's name
        // ending short of the ones beside it.
        let set_count = |unread: u32| {
            count.set_visible(unread > 0);
            if unread > 0 {
                count.set_label(&unread.to_string());
            }
        };
        match &*tree_item {
            TreeItem::Unified(unread) => {
                icon.set_visible(true);
                icon.set_icon_name(Some("mail-inbox-symbolic"));
                label.set_label("All Inboxes");
                label.set_css_classes(&["heading"]);
                set_count(*unread);
            }
            TreeItem::Favorites => {
                icon.set_visible(true);
                icon.set_icon_name(Some(themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])));
                label.set_label("Favorites");
                label.set_css_classes(&["heading"]);
                // A pure grouping row: every folder under it shows its own.
                set_count(0);
            }
            TreeItem::Account(account) => {
                icon.set_visible(false);
                label.set_label(&account.label);
                label.set_css_classes(&["heading"]);
                set_count(account.unread);
            }
            // A favorite renders exactly like the folder it duplicates.
            TreeItem::Folder(node) | TreeItem::Favorite(node) => {
                icon.set_visible(true);
                icon.set_icon_name(Some(folder_icon_name(node.mailbox.role)));
                label.set_label(&display_name(&node.mailbox.name));
                label.set_css_classes(&[]);
                set_count(node.mailbox.unread);
            }
        }
        // Refresh the row's drag-drop identity: folder rows accept moves
        // into that folder, everything else accepts nothing.
        if let Some(drop_data) = unsafe { expander.data::<Rc<RefCell<Option<Mailbox>>>>("lookout-drop-target") } {
            let mut slot = unsafe { drop_data.as_ref() }.borrow_mut();
            *slot = match &*tree_item {
                TreeItem::Folder(node) | TreeItem::Favorite(node) => Some(node.mailbox.clone()),
                _ => None,
            };
        }
        // Refresh the row's expunge quick action: Trash and Junk rows get the
        // button (with a role-named tooltip), everything else hides it.
        let role = match &*tree_item {
            TreeItem::Folder(node) | TreeItem::Favorite(node) => node.mailbox.role,
            _ => MailboxRole::Custom,
        };
        let expunge_eligible = matches!(role, MailboxRole::Trash | MailboxRole::Junk);
        if let Some(widgets) = unsafe { expander.data::<FolderRowWidgets>("lookout-expunge-widgets") } {
            let widgets = unsafe { widgets.as_ref() };
            widgets.expunge_enabled.set(expunge_eligible);
            // Defensive: a recycled row may still carry a visible box from a
            // stale hover; only hover on an eligible row ever re-shows it.
            if !expunge_eligible {
                widgets.action_box.set_visible(false);
            }
            *widgets.expunge_data.borrow_mut() = if expunge_eligible {
                match &*tree_item {
                    TreeItem::Folder(node) | TreeItem::Favorite(node) => Some(node.mailbox.clone()),
                    _ => None,
                }
            } else {
                None
            };
            widgets.expunge_btn.set_tooltip_text(Some(match role {
                MailboxRole::Trash => "Empty Trash",
                MailboxRole::Junk => "Empty Junk",
                _ => "Empty folder",
            }));
        }
    });

    let folder_list_view = gtk::ListView::new(Some(folder_selection.clone()), Some(folder_factory));
    let folder_scroller = gtk::ScrolledWindow::builder().child(&folder_list_view).vexpand(true).build();
    let folder_card = card_section(&folder_scroller);
    folder_card.add_css_class("folder-pane");
    folder_card.add_css_class("card-flush-end");
    folder_card.set_margin_end(0);
    // Floor on the folder pane's width, so the separator can't be dragged
    // left until the folder names are a sliver. Set on the card rather than
    // on `folder_scroller`'s child because `Gtk.ScrolledWindow` absorbs its
    // child's size request instead of propagating it - the same reason
    // `reading_stack`'s height floor is set where it is. It only bites
    // because `main_paned` sets `shrink_start_child(false)`, which is what
    // makes the Paned honour the child's minimum.
    folder_card.set_size_request(FOLDER_PANE_MIN_WIDTH, -1);

    // --- Message list ---
    let message_list = MessageListModel::build_with_worker(worker.clone());
    let last_selection = last_view::load(&settings);
    // Phase 5: the scalars that used to be hardcoded here now come from the
    // GSettings-backed store (see `settings`), so the session starts where
    // the last one ended. The store falls back to its schema defaults when
    // no schema is available, which is exactly these old values.
    let ui_db = crate::ui_state_db::UiStateDb::open()
        .map(Rc::new)
        .inspect_err(|e| tracing::warn!("starred contacts and trusted senders won't persist: {e}"))
        .ok();
    let starred_contacts: HashSet<(AccountId, String)> = ui_db
        .as_ref()
        .and_then(|db| db.load_starred().ok())
        .into_iter()
        .flat_map(|by_account| {
            by_account
                .into_iter()
                .flat_map(|(account, identities)| identities.into_iter().map(move |identity| (account.clone(), identity)))
        })
        .collect();
    let trusted_senders: HashMap<(AccountId, String), lookout_core::TrustLevel> = ui_db
        .as_ref()
        .and_then(|db| db.load_trusted_senders().ok())
        .into_iter()
        .flatten()
        .map(|(account, entry, level)| ((account, entry), level))
        .collect();
    // Shared keyring handle for manually-added ("other") IMAP/SMTP accounts -
    // see `other_accounts.rs`. Cloned into the add-account dialog, the
    // startup connect loop, and the Config view's edit/remove actions.
    let keyring = crate::other_accounts::SecretServiceKeyring::new();
    let state = Rc::new(RefCell::new(UiState {
        accounts: HashMap::new(),
        contacts_by_account: HashMap::new(),
        starred_contacts,
        ui_db,
        settings: settings.clone(),
        app_config: Rc::new(RefCell::new(crate::app_config::load())),
        deleted_contacts: HashMap::new(),
        contact_cmd_tx: HashMap::new(),
        goa_accounts: HashMap::new(),
        goa_client: None,
        current_account: None,
        current_mailbox: None,
        mail_view: MailView::Single,
        unified_snapshots: HashMap::new(),
        pending_optimistic_removals: HashMap::new(),
        pending_optimistic_flag_changes: HashMap::new(),
        pending_body_request: None,
        pending_attachment: None,
        pending_raw_message: None,
        unsubscribe_info: None,
        unsubscribe_dismissed: None,
        imip: None,
        imip_dismissed: None,
        read_receipt_request: None,
        read_receipt_dismissed: None,
        read_receipts_sent: HashSet::new(),
        read_receipt_context: None,
        pending_cid: HashMap::new(),
        rendered_inline_parts: Vec::new(),
        temp_attachment_files: HashSet::new(),
        toast_overlay: Some(toast_overlay.clone()),
        sending_toasts: HashMap::new(),
        pending_html_reveal: false,
        pending_header: None,
        body_cache: BodyCache::new(BODY_CACHE_IN_MEMORY),
        reveal_generation: 0,
        last_selection: last_selection.clone(),
        restore_pending: last_selection.is_some(),
        rendered_message: None,
        syncing: HashSet::new(),
        sort_key: SortKey::from_action_state(&settings.get_string(crate::settings::SORT_KEY)).unwrap_or(SortKey::Date),
        sort_descending: settings.get_bool(crate::settings::SORT_DESCENDING),
        favorites: settings.get_strv(crate::settings::MAIL_FAVORITES).into_iter().map(MailboxId).collect(),
        load_remote_images: settings.get_bool(crate::settings::MAIL_LOAD_REMOTE_IMAGES),
        rich_text_default: settings.get_bool(crate::settings::MAIL_RICH_TEXT_DEFAULT),
        trusted_senders,
        rendered_trust_sender: None,
        load_once_images: false,
        message_theme_override: false,
        trust_banner_dismissed: None,
        rendered_remote_scan: None,
        draft_saved_tx: None,
        composer_identities_refresh: None,
        composer_relay_generation: 0,
        compose_popout_window: None,
        folder_tree: None,
        suppress_folder_selection: false,
        search_active: false,
        search_query: String::new(),
        search_results: Vec::new(),
        search_pending: HashSet::new(),
    }));
    // The folder rows' drop targets reach `state` through this slot (they
    // were built before it existed).
    *state_slot.borrow_mut() = Some(state.clone());
    // Clean up the temporary files materialized for the attachment strip's
    // "Open" action when the app exits - the viewer process may still be
    // running, but Lookout must not leak temp files. (On Flatpak the portal
    // makes its own copy for the viewer, so removing ours is always safe.)
    let state_for_shutdown = state.clone();
    app.connect_shutdown(move |_| {
        let paths: Vec<PathBuf> = state_for_shutdown.borrow().temp_attachment_files.iter().cloned().collect();
        for path in paths {
            let _ = std::fs::remove_file(&path);
        }
    });
    let reading_stack = gtk::Stack::new();
    let state_clone = state.clone();
    let state_clone2 = state.clone();
    let reading_stack_clone = reading_stack.clone();
    // The shared tag-definition set, loaded from disk at startup. Mutated by
    // the Manage-tags dialog, read by the Categorize menu, the row context
    // menu, and every row's tag dots.
    let tags = Rc::new(RefCell::new(crate::tags::load()));
    // One provider encoding every tag's color as a `.message-tag-dot.tag-<key>`
    // rule, re-`load_from_string` whenever tags change (replacing the previous
    // rules wholesale). Registered once for the display's whole lifetime.
    let tag_colors = gtk::CssProvider::new();
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &tag_colors, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    apply_tag_colors(&tags, &tag_colors);
    let message_list_for_rows = message_list.clone();
    let tags_for_rows = tags.clone();
    let tags_for_bind = tags.clone();
    let tag_colors_for_rows = tag_colors.clone();
    // Declared here, ahead of the row factory below, so each row's `setup`
    // can bind its avatar/checkbox visibility to this button's `active`
    // state (see the property bindings inside `connect_setup`) - appended
    // into the message-list header, after the favorite star, further down
    // where the rest of that header's controls are built.
    let select_mode_button = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["checkbox-checked-symbolic", "list-add-symbolic", "edit-select-all-symbolic"]))
        .tooltip_text("Select messages")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    let select_mode_button_for_rows = select_mode_button.clone();
    let worker_for_rows = worker.clone();
    let message_factory = gtk::SignalListItemFactory::new();
    message_factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        // A row is either a date-section header or a message. Both branches
        // are built once here and toggled with `set_visible` rather than
        // being separate factories or a `Gtk.Stack` - a Stack would size
        // every row to the taller of the two, giving headers message height.
        let header_label = gtk::Label::builder().xalign(0.0).css_classes(["heading"]).build();
        let expander = gtk::TreeExpander::new();
        expander.set_child(Some(&header_label));
        let header_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(4)
            .margin_bottom(4)
            .margin_start(10)
            .margin_end(10)
            .build();
        header_box.add_css_class("message-section-header");
        header_box.append(&expander);

        // The unread accent bar: full row height, flush against the pane's
        // leading edge (which is why `.message-list > row` zeroes its
        // padding), colored only when the message is unread.
        let accent = gtk::Box::builder().width_request(3).vexpand(true).build();
        accent.add_css_class("message-accent-bar");
        let avatar = gtk::Label::builder()
            .css_classes(["avatar-circle"])
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .margin_start(8)
            .margin_end(8)
            .build();

        // Fixed-width sender, expanding subject: that's what lines every
        // row's subject up in one column, rather than letting it start
        // wherever the sender happens to end.
        let sender_label = gtk::Label::builder().xalign(0.0).width_request(180).ellipsize(gtk::pango::EllipsizeMode::End).build();
        // The attachment and meeting-invite indicators, drawn after the
        // sender column on rows that carry either. Same footprint as the flag
        // icon; hidden (not just blank) when absent so a plain row takes no
        // extra width.
        let attachment_icon = gtk::Image::builder()
            .icon_name("mail-attachment-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Has attachments")
            .visible(false)
            .build();
        let calendar_icon = gtk::Image::builder()
            .icon_name("x-office-calendar-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Meeting invite")
            .visible(false)
            .build();
        let subject_label = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let date_label = gtk::Label::builder().xalign(1.0).build();
        // Sits between the expanding subject and the date, so a flagged row
        // shows its marker without shifting the date column - hidden (not
        // just blank) on unflagged rows so it takes no width at all there.
        let flag_icon = gtk::Image::builder()
            .icon_name(themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"]))
            .pixel_size(12)
            .css_classes(["message-flag-icon"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        // One dot per configured color tag the message carries, filled in on
        // `bind`. Hidden when there are none (the container takes no space
        // when empty), so an untagged row is identical to today's.
        let tag_dots = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(3).valign(gtk::Align::Center).build();
        let top_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        top_row.append(&sender_label);
        top_row.append(&attachment_icon);
        top_row.append(&calendar_icon);
        top_row.append(&subject_label);
        top_row.append(&flag_icon);
        top_row.append(&tag_dots);
        top_row.append(&date_label);

        // Spans the full row and ellipsizes, so the snippet shows as much of
        // the body as the pane's current width fits and re-flows when the
        // pane is resized - the cached text is deliberately longer than any
        // width can display (see `PREVIEW_MAX_CHARS`).
        //
        // Always present, even when there's no preview text: an empty label
        // still reserves its line, so read and unread rows stay the same
        // height and the list doesn't ripple as previews arrive.
        let preview_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .css_classes(["message-preview"])
            .build();

        let text_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .margin_top(6)
            .margin_bottom(6)
            .margin_end(10)
            .build();
        text_column.append(&top_row);
        text_column.append(&preview_label);

        // Same footprint as the avatar it replaces (32x32, centered) so
        // toggling Select mode swaps one for the other in place rather than
        // adding a separate column - `bind_property` below (not bind()/CSS)
        // is what makes the swap live across every row, including ones
        // already on screen, the moment the header's Select toggle flips.
        let checkbox = gtk::CheckButton::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        // `sync_create` seeds each binding with the button's current state
        // immediately (rows created while already in Select mode - e.g. one
        // scrolled into view - must not default to showing the avatar), and
        // both bindings keep matching it live without any rebind: a plain
        // signal (message_factory has no per-toggle rebind hook) would only
        // reach rows bound *after* the flip.
        select_mode_button_for_rows
            .bind_property("active", &avatar, "visible")
            .invert_boolean()
            .sync_create()
            .build();
        select_mode_button_for_rows.bind_property("active", &checkbox, "visible").sync_create().build();

        let message_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
        message_box.add_css_class("message-row");
        message_box.append(&accent);
        message_box.append(&avatar);
        message_box.append(&checkbox);
        message_box.append(&text_column);

        // The conversation-header row: its own expander (the section header's
        // expander lives in `header_box`) driving a single sender/subject/date
        // line, with the participant count as the extra column that marks a
        // row as a thread rather than a message. Widths deliberately match
        // the message row's (`width_request(180)` sender, hexpand subject) so
        // threads and messages line up in the same columns.
        let thread_sender = gtk::Label::builder().xalign(0.0).width_request(180).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let thread_attachment_icon = gtk::Image::builder()
            .icon_name("mail-attachment-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Has attachments")
            .visible(false)
            .build();
        let thread_calendar_icon = gtk::Image::builder()
            .icon_name("x-office-calendar-symbolic")
            .pixel_size(12)
            .css_classes(["message-row-icon"])
            .valign(gtk::Align::Center)
            .tooltip_text("Meeting invite")
            .visible(false)
            .build();
        let thread_subject = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let thread_count = gtk::Label::builder().xalign(0.0).css_classes(["message-thread-count"]).build();
        let thread_flag = gtk::Image::builder()
            .icon_name(themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"]))
            .pixel_size(12)
            .css_classes(["message-flag-icon"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let thread_date = gtk::Label::builder().xalign(1.0).build();
        let thread_top = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        thread_top.append(&thread_sender);
        thread_top.append(&thread_attachment_icon);
        thread_top.append(&thread_calendar_icon);
        thread_top.append(&thread_subject);
        thread_top.append(&thread_count);
        thread_top.append(&thread_flag);
        thread_top.append(&thread_date);
        let thread_expander = gtk::TreeExpander::new();
        thread_expander.set_child(Some(&thread_top));
        let thread_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(14)
            .margin_end(10)
            .build();
        thread_box.add_css_class("message-thread-row");
        thread_box.append(&thread_expander);

        let row_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        row_box.append(&header_box);
        row_box.append(&message_box);
        row_box.append(&thread_box);

        // Action box (initially hidden)
        let action_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        action_box.add_css_class("hover-quick-actions");
        let archive_btn = gtk::Button::from_icon_name("mail-archive-symbolic");
        archive_btn.add_css_class("hover-quick-action");
        archive_btn.set_tooltip_text(Some("Archive"));
        let delete_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        delete_btn.add_css_class("hover-quick-action");
        delete_btn.set_tooltip_text(Some("Delete"));
        let reply_btn = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
        reply_btn.add_css_class("hover-quick-action");
        reply_btn.set_tooltip_text(Some("Reply"));
        action_box.append(&archive_btn);
        action_box.append(&delete_btn);
        action_box.append(&reply_btn);
        // Initially hide actions
        action_box.set_visible(false);
        // Overlay to overlay actions on top of content
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&row_box));
        overlay.add_overlay(&action_box);

        // Which message this row is currently showing. Written by `bind`,
        // read by the handlers below *at click time* - which is what lets
        // every signal be connected once here instead of on every rebind.
        // The old per-bind connections accumulated on recycled rows (nothing
        // ever disconnected them), so a scrolled list eventually fired one
        // click at several messages at once.
        let bound: Rc<RefCell<Option<EmailSummary>>> = Rc::new(RefCell::new(None));

        // The right-click (Categorize) popover for this row. Parented once to
        // the row's overlay; its contents are repopulated from the row's
        // current message at press time.
        let tag_popover = gtk::Popover::new();
        tag_popover.set_parent(&overlay);

        let checkbox_suppress: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            // Model -> checkbox, live: ctrl/shift-clicking a row already on
            // screen changes `list_item`'s `selected` property, which
            // `bind()` alone wouldn't see again until this row is recycled.
            let checkbox = checkbox.clone();
            let suppress = checkbox_suppress.clone();
            list_item.connect_selected_notify(move |li| {
                suppress.set(true);
                checkbox.set_active(li.is_selected());
                suppress.set(false);
            });
        }
        {
            // Checkbox -> model: an alternate input for the same selection
            // the row's own click/ctrl-click/shift-click already drives,
            // never a second, parallel selection concept. Weak, not a
            // strong clone: `list_item` already owns `checkbox` transitively
            // through the widget tree it holds as its child, so a strong
            // capture here would close a reference cycle
            // (list_item -> ... -> checkbox -> this closure -> list_item)
            // that would never be freed.
            let selection = message_list_for_rows.selection.clone();
            let list_item_weak = list_item.downgrade();
            let suppress = checkbox_suppress.clone();
            checkbox.connect_toggled(move |cb| {
                if suppress.get() {
                    return;
                }
                let Some(list_item) = list_item_weak.upgrade() else { return };
                let pos = list_item.position();
                if pos == gtk::INVALID_LIST_POSITION {
                    return;
                }
                if cb.is_active() {
                    selection.select_item(pos, false);
                } else {
                    selection.unselect_item(pos);
                }
            });
        }

        {
            let bound = bound.clone();
            let tag_popover = tag_popover.clone();
            let tags = tags_for_rows.clone();
            let state = state_clone.clone();
            let message_list = message_list_for_rows.clone();
            let tag_colors = tag_colors_for_rows.clone();
            let context_menu = gtk::GestureClick::new();
            context_menu.set_button(gtk::gdk::BUTTON_SECONDARY);
            context_menu.connect_pressed(move |_, _, x, y| {
                let Some(summary) = bound.borrow().clone() else { return };
                let boxed = build_tag_menu(&tags, &state, Some(summary), &message_list, &tag_colors);
                tag_popover.set_child(Some(&boxed));
                tag_popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                tag_popover.popup();
            });
            overlay.add_controller(context_menu);
        }

        // Drag source for every message row: dragging a message - or, when
        // the dragged row is part of a multi-selection, the whole selection -
        // offers two payloads in one provider union: the internal
        // `(mailbox, uid)` list for folder drops (see the folder rows' drop
        // target), and a `text/uri-list` of temp `.eml` files (only when the
        // raw bytes are already cached, since the drag must be synchronous)
        // so the drag can land in a file manager too. GTK's union provider
        // lets each drop target pick whichever format it understands.
        {
            let drag_source = gtk::DragSource::new();
            drag_source.set_actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE);
            let bound_for_drag = bound.clone();
            let state_for_drag = state_clone.clone();
            let message_list_for_drag = message_list_for_rows.clone();
            let drag_temp_files: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
            let drag_temp_files_for_prepare = drag_temp_files.clone();
            drag_source.connect_prepare(move |_source, _x, _y| {
                let summaries = dragged_summaries(&message_list_for_drag, &bound_for_drag);
                if summaries.is_empty() {
                    return None;
                }
                let internal: Vec<(String, u32)> = summaries.iter().map(|s| (s.mailbox.0.clone(), s.uid.0)).collect();
                let bytes = match serde_json::to_vec(&internal) {
                    Ok(bytes) => glib::Bytes::from(bytes.as_slice()),
                    Err(e) => {
                        tracing::warn!("failed to serialize message drag payload: {e}");
                        return None;
                    }
                };
                // The provider holds the payload as a `G_TYPE_BYTES` *value*
                // (rather than exposing it under a private mime type), because
                // GTK's drop targets only accept a drag when a GType can be
                // matched between the two sides' formats - a mime-only
                // `ContentFormats` has no gtypes, and an unregistered mime has
                // no GType mapping, so the folder rows would reject every
                // drop before `connect_drop` ever fired. The value is copied
                // straight into the drop handler on a local drop, no stream
                // round-trip needed.
                let internal_provider = gtk::gdk::ContentProvider::for_value(&glib::Value::from(bytes));
                Some(match write_external_drag_files(&state_for_drag, &summaries, &drag_temp_files_for_prepare) {
                    Some(files) => gtk::gdk::ContentProvider::new_union(&[internal_provider, files]),
                    None => internal_provider,
                })
            });
            // The temp `.eml` files written for the drag-out are cleaned up
            // once the drag finishes, whatever its outcome.
            {
                let drag_temp_files = drag_temp_files.clone();
                drag_source.connect_drag_end(move |_source, _drag, _delete| {
                    for path in drag_temp_files.borrow_mut().drain(..) {
                        let _ = std::fs::remove_file(path);
                    }
                });
            }
            overlay.add_controller(drag_source);
        }

        {
            let action_box = action_box.clone();
            let message_box = message_box.clone();
            let hover_controller = gtk::EventControllerMotion::new();
            let action_box_for_leave = action_box.clone();
            hover_controller.connect_enter(move |_, _, _| {
                // Section headers have no quick actions.
                if message_box.is_visible() {
                    action_box.set_visible(true);
                }
            });
            hover_controller.connect_leave(move |_| {
                action_box_for_leave.set_visible(false);
            });
            overlay.add_controller(hover_controller);
        }

        {
            let state = state_clone.clone();
            let bound = bound.clone();
            let message_list = message_list_for_rows.clone();
            archive_btn.connect_clicked(move |_| {
                let Some((mailbox, uid)) = bound.borrow().as_ref().map(|s| (s.mailbox.clone(), s.uid)) else {
                    return;
                };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
                if let Some(cmd_tx) = cmd_tx {
                    optimistic_remove_messages(&state, &message_list, &mailbox, &[uid]);
                    let _ = cmd_tx.send_blocking(AccountCommand::MoveMessage {
                        mailbox,
                        uid,
                        role: MailboxRole::Archive,
                    });
                }
            });
        }
        {
            let state = state_clone.clone();
            let bound = bound.clone();
            let message_list = message_list_for_rows.clone();
            delete_btn.connect_clicked(move |_| {
                let Some((mailbox, uid)) = bound.borrow().as_ref().map(|s| (s.mailbox.clone(), s.uid)) else {
                    return;
                };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
                if let Some(cmd_tx) = cmd_tx {
                    optimistic_remove_messages(&state, &message_list, &mailbox, &[uid]);
                    let _ = cmd_tx.send_blocking(AccountCommand::MoveMessage {
                        mailbox,
                        uid,
                        role: MailboxRole::Trash,
                    });
                }
            });
        }
        {
            let state = state_clone.clone();
            let worker = worker_for_rows.clone();
            let reading_stack = reading_stack_clone.clone();
            let bound = bound.clone();
            reply_btn.connect_clicked(move |_| {
                let Some(summary) = bound.borrow().clone() else { return };
                // Everything the composer needs is read out first and the
                // borrow dropped: `show_composer_in_reading_pane` takes its
                // own `borrow_mut` to install the draft-confirmation relay.
                let opened = {
                    let mut st = state.borrow_mut();
                    // Only possible once the body has arrived - the composer
                    // needs it to quote. Silently a no-op until then.
                    let body = st.body_cache.get(&summary.mailbox, &summary.uid);
                    body.and_then(|body| {
                        let account_id = mailbox_account_id(&summary.mailbox)?;
                        let handle = st.accounts.get(&account_id)?;
                        let from_email = handle.email.clone();
                        let cmd_tx = handle.cmd_tx.clone();
                        let prefill = crate::compose::build_reply_prefill(&summary, &body, &from_email, crate::compose::ReplyMode::Reply);
                        Some((from_email, cmd_tx, prefill, st.rich_text_default))
                    })
                };
                // Routed through the shared opener rather than building the
                // composer inline: that's what replaces any composer already
                // in the pane (so its autosave loop stops), restores the
                // previous page on close, and owns the `draft_saved_tx` slot.
                if let Some((_from_email, cmd_tx, prefill, rich_text_default)) = opened {
                    show_composer_in_reading_pane(&state, &worker, &reading_stack, "Reply", cmd_tx, prefill, rich_text_default, mailbox_account_id(&summary.mailbox));
                }
            });
        }

        unsafe {
            list_item.set_data(
                "row-widgets",
                MessageRowWidgets {
                    header_box,
                    expander,
                    header_label,
                    message_box,
                    thread_box,
                    thread_expander,
                    thread_sender,
                    thread_attachment_icon,
                    thread_calendar_icon,
                    thread_subject,
                    thread_count,
                    thread_flag,
                    thread_date,
                    checkbox,
                    checkbox_suppress,
                    accent,
                    avatar,
                    sender_label,
                    attachment_icon,
                    calendar_icon,
                    subject_label,
                    flag_icon,
                    tag_dots,
                    date_label,
                    preview_label,
                    action_box,
                    tag_popover,
                    bound,
                },
            );
        }
        list_item.set_child(Some(&overlay));
    });
    message_factory.connect_bind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else { return };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let widgets = unsafe { list_item.data::<MessageRowWidgets>("row-widgets").expect("row widgets").as_ref().clone() };
        let item = boxed.borrow::<MessageItem>();
        match &*item {
            MessageItem::Section(section) => {
                widgets.header_box.set_visible(true);
                widgets.message_box.set_visible(false);
                widgets.thread_box.set_visible(false);
                widgets.action_box.set_visible(false);
                widgets.tag_dots.set_visible(false);
                // What actually draws the disclosure chevron and drives
                // expand/collapse. The expansion *state* is applied in
                // `MessageListModel::repopulate`, not here - bind only runs
                // for rows in the viewport, so doing it here would leave
                // scrolled-off sections unexpanded and the list's row count
                // wrong.
                widgets.expander.set_list_row(Some(&row));
                widgets.header_label.set_label(&section.label);
                // Headers aren't a selection target for the mouse:
                // `set_selectable(false)` is a `ListItem` property enforced
                // by `GtkListView`'s own click/ctrl-click/shift-click
                // handling, independent of which `SelectionModel` backs the
                // view, so a header can never enter `MultiSelection`'s
                // selection set via the mouse. `SelectionKind::Section`
                // stays a defensive no-op case in the selection handler
                // regardless, in case a header is ever selected some other
                // way.
                list_item.set_selectable(false);
                list_item.set_activatable(false);
                *widgets.bound.borrow_mut() = None;
            }
            MessageItem::Message(summary) => {
                widgets.header_box.set_visible(false);
                widgets.message_box.set_visible(true);
                widgets.thread_box.set_visible(false);
                // Must be set explicitly, not just left alone: these are
                // `ListItem` properties that survive widget recycling, so a
                // row that last rendered a header would stay unclickable.
                list_item.set_selectable(true);
                list_item.set_activatable(true);
                // Authoritative reset on every bind, not just a one-time
                // connection: recycling a row must not leave it showing the
                // previous occupant's checked state, and this is cheaper
                // than reasoning about signal-timing races across recycles.
                widgets.checkbox_suppress.set(true);
                widgets.checkbox.set_active(list_item.is_selected());
                widgets.checkbox_suppress.set(false);

                let sender = summary.from.first();
                match sender {
                    Some(address) => {
                        widgets.avatar.set_label(&crate::message_header::initials(address.name.as_deref(), &address.address));
                        for class in crate::message_header::AVATAR_COLOR_CLASSES {
                            widgets.avatar.remove_css_class(class);
                        }
                        widgets.avatar.add_css_class(crate::message_header::avatar_color_class(&address.address));
                    }
                    None => widgets.avatar.set_label("?"),
                }

                let from = sender.map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
                // In the unified "All Inboxes" view, stamp each row with its
                // owning account so mail from mixed mailboxes stays readable.
                let account_label = {
                    let st = state_clone2.borrow();
                    if matches!(st.mail_view, MailView::UnifiedInbox) {
                        mailbox_account_id(&summary.mailbox).and_then(|id| st.accounts.get(&id)).map(|h| {
                            if h.display_name.is_empty() {
                                h.email.clone()
                            } else {
                                h.display_name.clone()
                            }
                        })
                    } else {
                        None
                    }
                };
                widgets.sender_label.set_label(&match account_label {
                    Some(acc) => format!("{from} · {acc}"),
                    None => from,
                });
                widgets.subject_label.set_label(summary.subject.as_deref().unwrap_or("(no subject)"));
                // The attachment and invite indicators: explicit on every
                // bind, so a recycled row never shows the previous
                // occupant's icons. Both are independent - a named `invite.ics`
                // attachment is both.
                widgets.attachment_icon.set_visible(summary.has_attachment);
                widgets.calendar_icon.set_visible(summary.has_calendar);
                widgets.date_label.set_label(&format_row_date(summary.date, chrono::Utc::now()));
                widgets.preview_label.set_label(summary.preview.as_deref().unwrap_or(""));

                let unread = summary.is_unread();
                widgets
                    .sender_label
                    .set_css_classes(if unread { &["message-sender-unread"] } else { &["message-sender-read"] });
                widgets
                    .subject_label
                    .set_css_classes(if unread { &["message-subject-unread"] } else { &["message-subject-read"] });
                widgets.date_label.set_css_classes(if unread {
                    &["caption", "message-date-unread"]
                } else {
                    &["caption", "message-date-read"]
                });
                if unread {
                    widgets.accent.add_css_class("unread");
                } else {
                    widgets.accent.remove_css_class("unread");
                }
                widgets.flag_icon.set_visible(summary.is_starred());

                // Rebuild the color-tag dots for this message. Colored by the
                // `.message-tag-dot.tag-<key>` rules `apply_tag_colors` keeps
                // in sync with the tag definitions.
                while let Some(child) = widgets.tag_dots.first_child() {
                    widgets.tag_dots.remove(&child);
                }
                let tags_borrow = tags_for_bind.borrow();
                let shown = crate::tags::tags_for_keywords(&tags_borrow, &summary.keywords);
                for tag in shown.iter().take(MAX_TAG_DOTS) {
                    let dot = gtk::Box::builder()
                        .width_request(8)
                        .height_request(8)
                        .css_classes(["message-tag-dot", &format!("tag-{}", tag.key)])
                        .build();
                    widgets.tag_dots.append(&dot);
                }
                widgets.tag_dots.set_visible(!shown.is_empty());

                *widgets.bound.borrow_mut() = Some((**summary).clone());
            }
            MessageItem::Thread(thread) => {
                widgets.header_box.set_visible(false);
                widgets.message_box.set_visible(false);
                widgets.thread_box.set_visible(true);
                widgets.action_box.set_visible(false);
                widgets.tag_dots.set_visible(false);
                // A conversation header is not a selection target, exactly
                // like a section header: clicking it expands/collapses (via
                // the `TreeExpander`) rather than selecting, and it can never
                // enter `MultiSelection`'s selection set. The batch actions'
                // checkboxes and the hover quick actions both key off the row
                // being a message, so a thread row shows neither.
                list_item.set_selectable(false);
                list_item.set_activatable(false);
                *widgets.bound.borrow_mut() = None;
                widgets.thread_expander.set_list_row(Some(&row));
                widgets.thread_sender.set_label(&thread.sender);
                widgets.thread_attachment_icon.set_visible(thread.has_attachment);
                widgets.thread_calendar_icon.set_visible(thread.has_calendar);
                widgets.thread_subject.set_label(thread.subject.as_deref().unwrap_or("(no subject)"));
                widgets.thread_count.set_label(&thread.count.to_string());
                // Who's in the conversation, a hover away - on the row itself
                // rather than the count badge, so the whole header answers it.
                widgets
                    .thread_box
                    .set_tooltip_text(Some(&format!("{} messages: {}", thread.count, thread.participants.join(", "))));
                widgets.thread_date.set_label(&format_row_date(thread.latest, chrono::Utc::now()));
                widgets.thread_flag.set_visible(thread.has_starred);
                // Unread bolds the whole header like it bolds a message row:
                // an unread member makes the conversation's sender, subject,
                // and date read as unread.
                let unread = thread.has_unread;
                widgets
                    .thread_sender
                    .set_css_classes(if unread { &["message-sender-unread"] } else { &["message-sender-read"] });
                widgets
                    .thread_subject
                    .set_css_classes(if unread { &["message-subject-unread"] } else { &["message-subject-read"] });
                widgets.thread_date.set_css_classes(if unread {
                    &["caption", "message-date-unread"]
                } else {
                    &["caption", "message-date-read"]
                });
            }
        }
    });
    message_factory.connect_unbind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(widgets) = (unsafe { list_item.data::<MessageRowWidgets>("row-widgets") }) else {
            return;
        };
        let widgets = unsafe { widgets.as_ref() };
        *widgets.bound.borrow_mut() = None;
        widgets.action_box.set_visible(false);
        widgets.tag_popover.popdown();
        // Don't keep a recycled row pinned to a `TreeListRow` it no longer
        // renders.
        widgets.expander.set_list_row(None);
        widgets.thread_expander.set_list_row(None);
    });
    let message_list_view = gtk::ListView::new(Some(message_list.selection.clone()), Some(message_factory));
    message_list_view.add_css_class("message-list");
    // Never scrolls sideways: every row's text ellipsizes to the pane's
    // width instead. Without pinning this, the preview line's natural width
    // request - it holds far more text than fits, by design, so the snippet
    // runs to the pane's edge at any width - would let the list report a
    // huge natural width and grow a horizontal scrollbar.
    let message_scroller = gtk::ScrolledWindow::builder()
        .child(&message_list_view)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    // Shown instead of the list while the folder on screen is empty and its
    // sync is still outstanding - see `refresh_message_loading_state`.
    // Without this, the empty list a folder switch eagerly paints (see
    // `select_mailbox`) looks identical to a folder that's genuinely empty
    // for however long the live IMAP fetch takes.
    let message_loading_spinner = gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build();
    let message_loading_label = gtk::Label::builder().label("Fetching message list.").css_classes(["dim-label"]).build();
    let message_loading_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build();
    message_loading_box.append(&message_loading_spinner);
    message_loading_box.append(&message_loading_label);
    let message_list_stack = gtk::Stack::new();
    message_list_stack.add_named(&message_scroller, Some("list"));
    message_list_stack.add_named(&message_loading_box, Some("loading"));
    message_list_stack.set_visible_child_name("list");
    // Header row atop the message list: what's being shown on the left (the
    // folder's name over its account, plus the favorite star), and the list's
    // own controls on the right - sync, filter, sort direction, sort key.
    // `title_column`'s hexpand is what pushes the control cluster to the
    // trailing edge, so no spacer widget is needed.
    let folder_title_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .label("Inbox")
        .build();
    let account_title_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let title_column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    title_column.append(&folder_title_label);
    title_column.append(&account_title_label);

    let favorite_button = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"]))
        .tooltip_text("Add to Favorites")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // `refresh_list_header` sets the star's state programmatically on every
    // view change; without this guard that would fire `toggled` and re-write
    // the favorites set from under the user.
    let favorite_suppress = Rc::new(Cell::new(false));

    let sync_button = gtk::Button::builder()
        .icon_name(themed_icon_name(&["view-refresh-symbolic", "emblem-synchronizing-symbolic"]))
        .tooltip_text("Sync")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // The filter menu, mirroring the sort-key one: a stateful action renders
    // the items as radio checks. `MessageListModel` owns the active filter -
    // it's applied inside `repopulate`, the single choke point every rebuild
    // passes through, against the model's unfiltered source of truth (see
    // `MessageListModel::set_filter`) - so the action just tells the model
    // which one to use.
    let list_filter_menu = gio::Menu::new();
    list_filter_menu.append(Some("All"), Some("win.list-filter('all')"));
    list_filter_menu.append(Some("Unread"), Some("win.list-filter('unread')"));
    list_filter_menu.append(Some("Flagged"), Some("win.list-filter('flagged')"));
    let list_filter_button = gtk::MenuButton::builder()
        .label(ListFilter::All.label())
        .tooltip_text("Filter")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&list_filter_menu)
        .build();
    let sort_direction_button = gtk::ToggleButton::builder()
        .icon_name(sort_direction_icon(true))
        .tooltip_text("Newest first")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .active(true)
        .build();
    let sort_key_menu = gio::Menu::new();
    sort_key_menu.append(Some("By Date"), Some("win.sort-key('date')"));
    sort_key_menu.append(Some("By Sender"), Some("win.sort-key('sender')"));
    sort_key_menu.append(Some("By Subject"), Some("win.sort-key('subject')"));
    let sort_key_button = gtk::MenuButton::builder()
        .label(state.borrow().sort_key.label())
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&sort_key_menu)
        .build();
    // The button itself (and the property bindings each row's avatar/
    // checkbox make to its `active` state) were built earlier, alongside
    // the row factory - see the comment there. All that's left here is the
    // one side effect that isn't per-row: clearing the selection on exit.
    {
        let message_list = message_list.clone();
        select_mode_button.connect_toggled(move |button| {
            if !button.is_active() {
                // Leaving Select mode clears the selection - matches Gmail's
                // "cancel selection" behavior and avoids stranding the
                // reading pane on the "N selected" placeholder after the
                // toggle that revealed it (and every row's checkbox) is gone.
                message_list.selection.unselect_all();
            }
        });
    }

    let message_header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(10)
        .margin_end(10)
        .margin_top(6)
        .margin_bottom(4)
        .build();
    message_header_row.append(&title_column);
    message_header_row.append(&favorite_button);
    message_header_row.append(&select_mode_button);
    message_header_row.append(&sync_button);
    message_header_row.append(&list_filter_button);
    message_header_row.append(&sort_direction_button);
    message_header_row.append(&sort_key_button);

    let list_header = ListHeader {
        folder_label: folder_title_label,
        account_label: account_title_label,
        favorite_button: favorite_button.clone(),
        favorite_suppress: favorite_suppress.clone(),
    };
    refresh_list_header(&state, &list_header);

    // Secondary header: the message list's column names, sitting directly
    // above the rows they name. It mirrors each row's internal geometry - a
    // gutter where the unread accent bar and avatar sit, then the fixed
    // sender column, the expanding subject column, and the right-aligned
    // date - so the titles line up exactly with the columns below.
    let column_header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_end(10)
        .css_classes(["message-column-header"])
        .build();
    // The sender column starts 51px in (3px accent + 8px margin + 32px avatar
    // + 8px margin); the box's own 8px spacing covers the last of that, so the
    // gutter is 43 wide.
    let avatar_gutter = gtk::Box::builder().width_request(43).build();
    column_header_row.append(&avatar_gutter);
    let sender_header = gtk::Label::builder().label("Sender").xalign(0.0).width_request(180).build();
    let subject_header = gtk::Label::builder().label("Subject").xalign(0.0).hexpand(true).build();
    let date_header = gtk::Label::builder().label("Date").xalign(1.0).build();
    column_header_row.append(&sender_header);
    column_header_row.append(&subject_header);
    column_header_row.append(&date_header);

    // --- Full-text search: a permanent `gtk::SearchEntry` in the window
    // header bar (see the header section below), wired up after the nav-rail
    // module buttons exist so a search started from any module lands on Mail.
    // Typing (debounced) swaps the list into search mode - instant results
    // from the local FTS index, with the open mailbox's live IMAP pass
    // catching up a beat later. Esc or clearing the text leaves search mode
    // and restores the previous view. The entry is built here, next to the
    // state it searches, and parented to the header later.
    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Search mail")
        .width_request(360)
        .tooltip_text("Search mail (Ctrl+F)")
        .css_classes(["header-search-entry"])
        .build();
    // A debounce token bumped on every keystroke and captured by each
    // scheduled timeout, so a timeout whose token is stale knows the user has
    // typed again and its query is superseded (the newer keystroke armed a
    // newer timeout).
    let search_debounce: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    let message_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_box.append(&message_header_row);
    message_box.append(&column_header_row);
    message_box.append(&message_list_stack);
    let message_card = card_section(&message_box);
    message_card.add_css_class("card-flush-start");
    message_card.set_margin_start(0);
    // Halves the gap to the reading pane: that gap is this margin plus the
    // Paned separator (12px, see `install_paned_css`) plus the reading
    // card's own start margin, so dropping both card margins to 0 takes it
    // from 24px to 12px. Zeroing the margins rather than narrowing the
    // separator keeps the whole gap draggable - the separator is
    // transparent, so the two look identical but only it grabs the pointer.
    message_card.set_margin_end(0);

    // --- Sync button -> re-sync whatever the list is showing. ---
    {
        let state = state.clone();
        sync_button.connect_clicked(move |_| resync_current_view(&state));
    }

    // --- Sort direction toggle -> flip the order and re-sort in place. The
    // re-sort reads the visible list back out of the model rather than
    // re-fetching, so it costs nothing and keeps the selection (see
    // `MessageListModel::displayed_messages`). ---
    {
        // Phase 5: put the button back on the persisted direction before the
        // handler below is wired, so startup doesn't write anything through.
        let descending = state.borrow().sort_descending;
        sort_direction_button.set_active(descending);
        sort_direction_button.set_icon_name(sort_direction_icon(descending));
        sort_direction_button.set_tooltip_text(Some(if descending { "Newest first" } else { "Oldest first" }));
    }
    {
        let state = state.clone();
        let message_list = message_list.clone();
        sort_direction_button.connect_toggled(move |button| {
            let descending = button.is_active();
            button.set_icon_name(sort_direction_icon(descending));
            button.set_tooltip_text(Some(if descending { "Newest first" } else { "Oldest first" }));
            state.borrow_mut().sort_descending = descending;
            state.borrow().settings.set_bool(crate::settings::SORT_DESCENDING, descending);
            resort_message_list(&state, &message_list);
        });
    }

    // --- Sort key -> a stateful action so the menu renders radio checks. The
    // action is added to the window once it exists (see `sort_key_action`
    // below); menu actions resolve through the widget hierarchy at activation
    // time, so registering it after the menu is built is fine. ---
    let sort_key_action = gio::SimpleAction::new_stateful("sort-key", Some(glib::VariantTy::STRING), &state.borrow().sort_key.action_state().to_variant());
    {
        let state = state.clone();
        let message_list = message_list.clone();
        let sort_key_button = sort_key_button.clone();
        sort_key_action.connect_activate(move |action, parameter| {
            let Some(key) = parameter.and_then(|p| p.str()).and_then(SortKey::from_action_state) else {
                return;
            };
            action.set_state(&key.action_state().to_variant());
            sort_key_button.set_label(key.label());
            state.borrow_mut().sort_key = key;
            state.borrow().settings.set_string(crate::settings::SORT_KEY, key.action_state());
            resort_message_list(&state, &message_list);
        });
    }

    // --- Filter -> a stateful action so the menu renders radio checks,
    // mirroring the sort-key action above. The active filter lives in the
    // model, so changing it just re-renders the list from the model's
    // unfiltered source of truth (see `MessageListModel::set_filter`). ---
    let list_filter_action = gio::SimpleAction::new_stateful("list-filter", Some(glib::VariantTy::STRING), &ListFilter::All.action_state().to_variant());
    {
        let message_list = message_list.clone();
        let list_filter_button = list_filter_button.clone();
        list_filter_action.connect_activate(move |action, parameter| {
            let Some(filter) = parameter.and_then(|p| p.str()).and_then(ListFilter::from_action_state) else {
                return;
            };
            action.set_state(&filter.action_state().to_variant());
            list_filter_button.set_label(filter.label());
            message_list.set_filter(filter);
        });
    }

    // --- Favorite star -> add/remove the open folder from the tree's
    // Favorites section. Session-only (see `UiState::favorites`). ---
    {
        let state = state.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let suppress = favorite_suppress.clone();
        favorite_button.connect_toggled(move |button| {
            if suppress.get() {
                return;
            }
            let Some(mailbox) = state.borrow().current_mailbox.clone() else { return };
            {
                let mut st = state.borrow_mut();
                if button.is_active() {
                    st.favorites.insert(mailbox.clone());
                } else {
                    st.favorites.remove(&mailbox);
                }
            }
            // Phase 5: write the whole favorites set through, so the tree's
            // Favorites section survives restarts.
            let favorites: Vec<String> = state.borrow().favorites.iter().map(|m| m.0.clone()).collect();
            state.borrow().settings.set_strv(crate::settings::MAIL_FAVORITES, favorites);
            apply_favorite_visual(button, button.is_active());
            // The tree grows/loses a whole section, so it has to be rebuilt -
            // which swaps the model and drops the highlight. Put it back on the
            // folder the user is still looking at.
            rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
            if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                if let Some(index) = find_mailbox_index(&model, &mailbox) {
                    folder_selection.set_selected(index);
                }
            }
        });
    }

    let compose_button = gtk::Button::from_icon_name("mail-message-new-symbolic");
    compose_button.set_tooltip_text(Some("New Message"));

    // Dev-only: load a raw .eml fixture straight into the reading pane,
    // bypassing IMAP entirely - for manually exercising render_body()
    // against test-fixtures/. Compiled out of release builds.
    #[cfg(debug_assertions)]
    let open_eml_button = gtk::Button::from_icon_name("document-open-symbolic");
    #[cfg(debug_assertions)]
    open_eml_button.set_tooltip_text(Some("Open .eml (debug)"));

    // --- Reading pane: WebKit for HTML, GtkTextView for plain text ---
    let _reading_stack = reading_stack.clone();
    let webkit_settings = webkit::Settings::new();
    webkit_settings.set_enable_javascript(false);
    webkit_settings.set_enable_developer_extras(false);
    // The "Switch message theme" toggle lives on the user content manager:
    // its override stylesheet is added/removed there so WebKit re-applies the
    // style to the document already on screen the moment the toggle flips
    // (the toggle handler also re-renders, see below). Built here, empty, so
    // the toggle's closure can arm it later.
    let user_content_manager = webkit::UserContentManager::new();
    let theme_override_sheet = webkit::UserStyleSheet::new(
        MESSAGE_THEME_OVERRIDE_CSS,
        webkit::UserContentInjectedFrames::TopFrame,
        webkit::UserStyleLevel::User,
        &[],
        &[],
    );
    let web_view = webkit::WebView::builder()
        .settings(&webkit_settings)
        .user_content_manager(&user_content_manager)
        .hexpand(true)
        .vexpand(true)
        .build();
    // Inline `cid:` image resolution: register a custom URI scheme so WebKit
    // asks us for the bytes behind `<img src="cid:...">` references instead
    // of failing the load. The handler callback fires on a WebKit worker
    // thread, so it must not touch `UiState` - it only forwards the request
    // to the main loop, which matches it against the rendered message's
    // inline parts and fetches the bytes on demand (`FetchAttachment`, served
    // from the flat-file cache on repeat visits).
    //
    // Deliberately *not* calling `WebKitSecurityManager::register_uri_scheme_
    // as_local` here - despite how it reads, "local" is a restriction, not a
    // grant: it means *other* non-local pages are forbidden from linking to
    // this scheme, not that this scheme is reachable from anywhere. The
    // message body is loaded via `load_html(html, None)`, which gives the
    // page a non-local origin - so marking "cid" local was actively cutting
    // the page's own image tags off from the scheme meant to serve them,
    // which is exactly why no inline image ever loaded. A bare
    // `register_uri_scheme` with no security-manager registration is what
    // makes the scheme reachable from any page, local or not.
    let (cid_tx, cid_rx) = async_channel::unbounded();
    if let Some(context) = web_view.context() {
        context.register_uri_scheme("cid", move |request| {
            let Some(cid) = request.path() else { return };
            let _ = cid_tx.send_blocking(CidSchemeRequest {
                request: SendWrapper(request.clone()),
                cid: cid.to_string(),
            });
        });
    }
    // The scheme handler's main-loop half: resolve each forwarded `cid:`
    // reference against the message currently on the reading pane and fetch
    // the matching part's bytes. This task outlives any single message, so
    // the resolution is always validated against `rendered_message`/`pending_cid`
    // at the moment the request lands.
    {
        let state_for_cid = state.clone();
        glib::spawn_future_local(async move {
            while let Ok(request) = cid_rx.recv().await {
                dispatch_cid_request(&state_for_cid, &request.cid, request.request.0);
            }
        });
    }
    // Block navigation *away* from the loaded message body (e.g. clicking a
    // link) - but NOT the initial programmatic `load_html()` call itself,
    // which also fires a NavigationAction decision. Distinguish the two via
    // `is_user_gesture()` (and `NavigationType::LinkClicked`, which is more
    // reliable across WebKitGTK versions): a real click is a user gesture,
    // `load_html()` is not. Getting this wrong (blocking unconditionally)
    // silently vetoes every load, which is exactly the "reading pane always
    // blank" bug this fixes - the WebView was never rendering anything
    // because its own initial content load was being cancelled before it
    // started. A clicked link is handed off to the system's default browser
    // instead of being dropped, and `target="_blank"` links - which WebKit
    // reports as a *new-window* decision rather than a navigation - are
    // routed to the browser by the `create` handler below.
    //
    // Remote *subresources* (tracker pixels, remote images/fonts, `<iframe>`s
    // pointing at outside URLs) are vetoed outright: they'd let external
    // servers reach into the pane, and - the reason this matters for speed -
    // `render_body` only reveals the page once WebKit reports `Finished`,
    // which the document's load event waits on. A slow remote host can hold
    // the reading pane blank for seconds; blocking these loads keeps the
    // reveal fast. `data:` (the body itself), `cid:` (inline parts, not yet
    // resolved but harmless) and `about:`/`file:` are all local and allowed.
    // Config → Mail's "Load images from the web" relaxes the response veto
    // for `image/*` subresources specifically; everything else (scripts,
    // fonts, iframes) stays blocked, and the veto still applies to the
    // navigation branch so an embedded remote `<iframe>` can't load either.
    // The preference lives in `UiState::load_remote_images` - the single
    // source of truth the Config toggle flips - and is re-read on every
    // decision, so the viewer always reflects the current setting.
    //
    // The external-content trust flow extends the same predicate per
    // message: `render_body` stashes the rendered message's sender in
    // `UiState::rendered_trust_sender`, and the sender's entry in
    // `UiState::trusted_senders` (an exact address or `@domain`, per
    // receiving account, persisted in the UI-state database) raises the
    // bar to the entry's `TrustLevel`. State is re-read on every decision
    // too, so trusting a sender while a message is open applies on the
    // next render.
    // Opens an `http(s)` URI in the system's default browser. Other schemes
    // are deliberately ignored - `data:`/`cid:`/`about:`/`file:` are local to
    // the pane, and `mailto:` is itself a mail client's business (a
    // compose-from-link flow is the later refinement).
    fn open_uri_in_default_browser(uri: &str) {
        let scheme = uri.split(':').next().unwrap_or("");
        if matches!(scheme, "http" | "https") {
            if let Err(e) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
                tracing::warn!("failed to open link {uri} in the default browser: {e}");
            }
        }
    }
    let state_for_policy = state.clone();
    web_view.connect_decide_policy(move |_view, decision, decision_type| {
        let uri_is_local = |uri: &str| -> bool {
            let scheme = uri.split(':').next().unwrap_or("");
            matches!(scheme, "data" | "cid" | "about" | "file")
        };
        match decision_type {
            webkit::PolicyDecisionType::NavigationAction => {
                let navigation = decision.downcast_ref::<webkit::NavigationPolicyDecision>().and_then(|d| d.navigation_action());
                // A real click on a link: block the in-pane navigation so the
                // message body stays on screen, but hand the target off to the
                // system's default browser instead of dropping it. Keyed on
                // the navigation type (`LinkClicked`) rather than
                // `is_user_gesture()` alone, which is unreliable across
                // WebKitGTK versions. `target="_blank"` links never arrive
                // here at all - WebKit reports them as a *new-window*
                // decision, handled below via the `create` signal.
                let is_link_click = navigation.as_ref().map(|a| a.navigation_type() == webkit::NavigationType::LinkClicked).unwrap_or(false);
                let is_user_gesture = navigation.as_ref().map(|a| a.is_user_gesture()).unwrap_or(false);
                if is_link_click || is_user_gesture {
                    if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                        open_uri_in_default_browser(&uri);
                    }
                    decision.ignore();
                    return true;
                }
                // A programmatic navigation (e.g. an `<iframe>` embedded in
                // the message pointing at a remote URL) must not load. The
                // initial `load_html()` is a `data:` URL and passes below.
                if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                    if !uri_is_local(&uri) {
                        decision.ignore();
                        return true;
                    }
                }
            }
            webkit::PolicyDecisionType::NewWindowAction => {
                // `target="_blank"` links (and middle-click opens) arrive as
                // a new-window request, not a navigation. These bindings
                // can't read the target URL off a new-window decision, so
                // allow it through and let the `create` handler - connected
                // below - route the URL to the default browser and swallow
                // the window request.
                return false;
            }
            webkit::PolicyDecisionType::Response => {
                // Veto remote subresource responses (images, fonts, scripts)
                // so the page's load event isn't held hostage by external
                // servers. The main frame's own resource - the `data:` body
                // URL - is always let through. "Load images from the web"
                // (Config → Mail) allows remote `image/*` responses; the
                // external-content trust flow (the "Trust sender" banner)
                // additionally lets a trusted sender's message load images,
                // or - at the `AllContent` level - every remote subresource
                // response. Everything else stays blocked, and the veto
                // still applies to the navigation branch above.
                if let Some(response) = decision.downcast_ref::<webkit::ResponsePolicyDecision>() {
                    if !response.is_main_frame_main_resource() {
                        if let Some(uri) = response.request().and_then(|r| r.uri()) {
                            if !uri_is_local(&uri) {
                                let is_image = response.response().and_then(|r| r.mime_type()).is_some_and(|m| m.starts_with("image/"));
                                let st = state_for_policy.borrow();
                                // The rendered message's sender (re-stashed
                                // by `render_body`) resolves its trust level;
                                // a `@domain` entry covers every address on
                                // that domain, and the strongest matching
                                // entry wins (an exact address outranks the
                                // account's broader domain entry).
                                let level = st.rendered_trust_sender.as_ref().and_then(|(account, sender)| {
                                    st.trusted_senders
                                        .iter()
                                        .filter(|((acc, entry), _)| acc == account && lookout_core::sender_matches_trust_entry(sender, entry))
                                        .map(|(_, level)| *level)
                                        .max()
                                });
                                let images_ok = st.load_remote_images || st.load_once_images || level.is_some_and(|l| l >= lookout_core::TrustLevel::Images);
                                let all_ok = level.is_some_and(|l| l >= lookout_core::TrustLevel::AllContent);
                                if !((images_ok && is_image) || all_ok) {
                                    decision.ignore();
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        false
    });

    // `target="_blank"` links (and middle-click / modifier-click opens) reach
    // this handler: the `NewWindowAction` decision above allowed them so the
    // URL - unreadable off the decision in these bindings - arrives here on
    // the navigation action. A link click routes the URL to the default
    // browser; the window request is swallowed either way (returning `None`
    // aborts the new-window creation, so nothing opens inside the pane).
    web_view.connect_create(move |_view, navigation_action| {
        let is_link_click = navigation_action.navigation_type() == webkit::NavigationType::LinkClicked;
        let is_user_gesture = navigation_action.is_user_gesture();
        if is_link_click || is_user_gesture {
            if let Some(uri) = navigation_action.request().and_then(|r| r.uri()) {
                open_uri_in_default_browser(&uri);
            }
        }
        None
    });

    let text_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let text_scroller = gtk::ScrolledWindow::builder().child(&text_view).build();

    // --- Reading-pane header: subject/sender/avatar/To/date, plus a second
    // set of Reply/Reply-All/Forward buttons duplicating the top command
    // toolbar's (see below). Lives inside the "message" page below, so it's
    // visible only while an actual message is shown, not for the empty
    // placeholder or the in-place composer.
    let message_header = crate::message_header::build();

    // The reading pane crossfades between three top-level pages: a single
    // "message" page that groups the message header with the body - so both
    // fade out/in together instead of the header popping out of sync with
    // the crossfading content - plus the "empty" placeholder and the
    // in-place composer. Inside the message page, a small no-transition
    // content stack toggles between the HTML web view and the plain-text
    // view without disturbing the outer crossfade.
    let reading_stack = gtk::Stack::new();
    let content_stack = gtk::Stack::new();
    content_stack.set_widget_name("body");
    content_stack.set_transition_type(gtk::StackTransitionType::None);
    content_stack.add_named(&web_view, Some("html"));
    content_stack.add_named(&text_scroller, Some("text"));
    let message_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_page.append(&message_header.subject_bar);
    message_page.append(&message_header.widget);
    // The attachment strip and body content stack are named children -
    // `find_named_child` locates them by walk rather than relying on sibling
    // order, so `action_bar` can sit after the body without breaking
    // `render_body`'s lookup. The strip is hidden (and emptied) when the
    // message has no attachments.
    let attachment_strip = gtk::Box::new(gtk::Orientation::Vertical, 6);
    attachment_strip.set_widget_name("attachments");
    attachment_strip.set_margin_start(12);
    attachment_strip.set_margin_end(12);
    attachment_strip.set_margin_top(8);
    attachment_strip.set_margin_bottom(4);
    attachment_strip.set_visible(false);
    message_page.append(&attachment_strip);
    // The iMIP invite-details card, between the attachments and the body.
    // The `adw::Banner` at the bottom of the page only fits a title and a
    // button, so this card carries what the invitation actually says - when,
    // where, who's organizing, and any description - with one row per detail
    // (`render_invite_card` shows/hides rows as the payload provides them).
    // Built once here as a named child; `render_body` repopulates it per
    // message, mirroring the attachment strip's lifecycle.
    let imip_invite_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["card"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    imip_invite_card.set_widget_name("imip-invite-card");
    imip_invite_card.set_visible(false);
    imip_invite_card.append(&invite_detail_row("imip-when-row", "imip-when", "When"));
    imip_invite_card.append(&invite_detail_row("imip-where-row", "imip-where", "Where"));
    imip_invite_card.append(&invite_detail_row("imip-organizer-row", "imip-organizer", "Organizer"));
    imip_invite_card.append(&invite_detail_row("imip-description-row", "imip-description", "Description"));
    message_page.append(&imip_invite_card);
    message_page.append(&content_stack);
    message_page.append(&message_header.action_bar);
    // The List-Unsubscribe banner, between the header and the body: revealed
    // by `render_body` when the message's headers offer an unsubscribe
    // action (RFC 2369 / one-click RFC 8058), hidden otherwise and once the
    // user dismisses it. A named child so `render_body` can find it.
    let unsubscribe_banner = adw::Banner::new("Unsubscribe from this mailing list?");
    unsubscribe_banner.set_widget_name("unsubscribe-banner");
    unsubscribe_banner.set_button_label(Some("Unsubscribe"));
    unsubscribe_banner.set_revealed(false);
    message_page.append(&unsubscribe_banner);
    // The iMIP banner (invitations / cancellations / RSVP replies carried as
    // `text/calendar` parts), between the header and the body like the
    // unsubscribe banner: revealed by `render_body` when the message carries
    // an iMIP payload the user hasn't dismissed, hidden otherwise. Its button
    // opens the per-method action dialog (REQUEST: Accept/Maybe/Decline,
    // CANCEL: remove-from-calendar confirm, REPLY: plain dismiss) - see the
    // handler registered once `calendar_state` exists below.
    let imip_banner = adw::Banner::new("Invitation");
    imip_banner.set_widget_name("imip-banner");
    imip_banner.set_button_label(Some("Respond…"));
    imip_banner.set_revealed(false);
    message_page.append(&imip_banner);
    // The external-content trust banner, between the header and the body
    // like the others: revealed by `render_body` when the message on screen
    // references remote content the load policy is blocking for its sender,
    // hidden otherwise and once the user acts on it. Its button opens the
    // trust dialog (load once / trust images / trust everything) - see the
    // handler registered below.
    let trust_banner = adw::Banner::new("Remote content is blocked");
    trust_banner.set_widget_name("trust-banner");
    trust_banner.set_button_label(Some("Trust sender…"));
    trust_banner.set_revealed(false);
    message_page.append(&trust_banner);
    // The read-receipt banner (RFC 8098 `Disposition-Notification-To`),
    // between the header and the body like the others: revealed by
    // `render_body` when the message on screen requests a read receipt and
    // the automatic policy is off (when it's on, the receipt is sent
    // silently instead). Its button sends the receipt - see the handler
    // registered below.
    let read_receipt_banner = adw::Banner::new("This message requests a read receipt");
    read_receipt_banner.set_widget_name("read-receipt-banner");
    read_receipt_banner.set_button_label(Some("Send read receipt"));
    read_receipt_banner.set_revealed(false);
    message_page.append(&read_receipt_banner);
    reading_stack.add_named(&message_page, Some("message"));
    let reading_empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reading_stack.add_named(&reading_empty, Some("empty"));
    // A separate page from "empty" (which several call sites already treat
    // as a true blank state) - "nothing selected" and "several messages
    // deliberately selected" are different states to the user even though
    // both mean "no single body to show," so this gets its own name rather
    // than overloading "empty" with a sometimes-visible label.
    let reading_multi_label = gtk::Label::builder()
        .css_classes(["dim-label"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    let reading_multi = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).vexpand(true).build();
    reading_multi.append(&reading_multi_label);
    reading_stack.add_named(&reading_multi, Some("multi"));
    reading_stack.set_visible_child_name("empty");
    // Interpolated crossfade between the reading pane's pages so a
    // message's header + body fade out and the next fades in instead of
    // snapping. Message switches already pass through the "empty" page
    // (the selection handler flips there before the body arrives), so both
    // halves of the transition fire for free - `render_body` handles the
    // same-page re-render case by routing through "empty" explicitly.
    reading_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    reading_stack.set_transition_duration(100);
    // --- List-Unsubscribe banner actions. The banner's button acts on
    // whatever message is on the reading pane (its parsed unsubscribe info is
    // stashed in `UiState::unsubscribe_info` by `render_body`): a one-click
    // POST (RFC 8058) when the list advertises one, degrading to the
    // `mailto:` action when the POST fails or no URL is offered. Acting on
    // the offer dismisses the banner for that message (Adw.Banner has no
    // close button of its own); a failed POST with no mailto fallback keeps
    // it visible so the user can try again.
    {
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        unsubscribe_banner.connect_button_clicked(move |banner| {
            let (mailbox, uid, list, cmd_tx) = {
                let st = state.borrow();
                let (mailbox, uid) = match &st.rendered_message {
                    Some(rendered) => rendered.clone(),
                    None => return,
                };
                let Some(list) = st.unsubscribe_info.clone() else { return };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let Some(handle) = st.accounts.get(&account_id) else { return };
                (mailbox, uid, list, handle.cmd_tx.clone())
            };
            let dismiss = |state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid| {
                state.borrow_mut().unsubscribe_dismissed = Some((mailbox.clone(), uid));
            };
            if list.one_click {
                if let Some(url) = list.http.clone() {
                    // One-click POST: disable the button while in flight so a
                    // double-click can't send twice.
                    banner.set_sensitive(false);
                    let state_for_post = state.clone();
                    let worker_for_post = worker.clone();
                    let reading_stack_for_post = reading_stack.clone();
                    let banner_for_post = banner.clone();
                    glib::spawn_future_local(async move {
                        let result = post_one_click_unsubscribe(&url).await;
                        banner_for_post.set_sensitive(true);
                        match result {
                            Ok(()) => {
                                dismiss(&state_for_post, &mailbox, uid);
                                banner_for_post.set_revealed(false);
                                if let Some(overlay) = &state_for_post.borrow().toast_overlay {
                                    overlay.add_toast(adw::Toast::new("Unsubscribed"));
                                }
                            }
                            Err(message) => {
                                if let Some(overlay) = &state_for_post.borrow().toast_overlay {
                                    let title = glib::markup_escape_text(&format!("Couldn't unsubscribe: {message}"));
                                    overlay.add_toast(adw::Toast::new(&title));
                                }
                                // The list may still accept the RFC 2369
                                // mailto path - degrade to it rather than
                                // leaving the user with nothing.
                                if let Some(mailto) = list.mailto.clone() {
                                    dismiss(&state_for_post, &mailbox, uid);
                                    banner_for_post.set_revealed(false);
                                    open_mailto_unsubscribe(&state_for_post, &worker_for_post, &reading_stack_for_post, mailto, cmd_tx, mailbox_account_id(&mailbox));
                                }
                            }
                        }
                    });
                    return;
                }
            }
            if let Some(mailto) = list.mailto.clone() {
                dismiss(&state, &mailbox, uid);
                banner.set_revealed(false);
                open_mailto_unsubscribe(&state, &worker, &reading_stack, mailto, cmd_tx, mailbox_account_id(&mailbox));
            }
        });
    }
    // --- External-content trust banner actions. The banner's button acts on
    // the sender of whatever message is on the reading pane (stashed in
    // `UiState::rendered_trust_sender` by `render_body`): a three-way dialog
    // lets the user load the message's remote images just this once, trust
    // the sender's images, or trust all of the sender's remote content. Trust
    // is persisted in the UI-state database and written through to
    // `trusted_senders`; every action dismisses the banner for that message
    // (Adw.Banner has no close button of its own) and re-renders the body so
    // the new policy applies to the open message immediately.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        let message_list = message_list.clone();
        trust_banner.connect_button_clicked(move |banner| {
            let (mailbox, uid) = {
                let st = state.borrow();
                let (mailbox, uid) = match &st.rendered_message {
                    Some(rendered) => rendered.clone(),
                    None => return,
                };
                let Some((_account, _sender)) = st.rendered_trust_sender.clone() else { return };
                (mailbox, uid)
            };
            let dialog = adw::AlertDialog::builder()
                .heading("Remote content blocked")
                .body("This message references content hosted on remote servers, which Lookout blocks. You can load it once, or trust this sender to load it from now on.")
                .default_response("images")
                .close_response("cancel")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("once", "Load images once");
            dialog.add_response("images", "Trust sender (images only)");
            dialog.add_response("all", "Trust sender (all content)");
            let state_for_dialog = state.clone();
            let reading_stack_for_dialog = reading_stack.clone();
            let message_header_for_dialog = message_header.clone();
            let message_list_for_dialog = message_list.clone();
            let banner_for_dialog = banner.clone();
            dialog.connect_response(None, move |_dialog, response| {
                if response == "cancel" {
                    return;
                }
                {
                    let mut st = state_for_dialog.borrow_mut();
                    st.trust_banner_dismissed = Some((mailbox.clone(), uid));
                    st.load_once_images = response == "once";
                    if response == "images" || response == "all" {
                        let level = if response == "all" {
                            lookout_core::TrustLevel::AllContent
                        } else {
                            lookout_core::TrustLevel::Images
                        };
                        if let Some((account, sender)) = st.rendered_trust_sender.clone() {
                            if let Some(db) = &st.ui_db {
                                let _ = db.set_trusted_sender(&account, &sender, level);
                            }
                            st.trusted_senders.insert((account, sender), level);
                        }
                    }
                }
                banner_for_dialog.set_revealed(false);
                rerender_current_message(&state_for_dialog, &reading_stack_for_dialog, &message_header_for_dialog, &message_list_for_dialog);
            });
            dialog.present(Some(banner));
        });
    }
    // --- Read-receipt banner actions. The banner's button sends the RFC
    // 8098 receipt for whatever message is on the reading pane (the request
    // and context stashed by `render_body`): one click, one receipt, and the
    // message is marked receipted so re-opening it won't offer again - the
    // same per-message dismissal convention as the other banners.
    {
        let state = state.clone();
        read_receipt_banner.connect_button_clicked(move |banner| {
            let sent = send_read_receipt(&state, false);
            if sent {
                banner.set_revealed(false);
            }
        });
    }
    // WebKit paints asynchronously - revealing the HTML page while a fresh
    // body is still loading would show a blank/white page before the message
    // appears. So the body is loaded while the pane holds on "empty", and a
    // single persistent `load-changed` handler reveals the page only once the
    // load completes. `render_body` arms this with `pending_html_reveal`; the
    // selection handler disarms it whenever the user moves on, so a load
    // started for a stale message can never yank the pane back open (the
    // older per-render one-shot handlers instead fired on *any* Finished and
    // caused double reveals / stale emails appearing).
    let state_for_reveal = state.clone();
    let content_stack_for_reveal = content_stack.clone();
    let reading_stack_for_reveal = reading_stack.clone();
    web_view.connect_load_changed(move |_, event| {
        if event != webkit::LoadEvent::Finished {
            return;
        }
        let (armed, stuck_on_empty) = {
            let st = state_for_reveal.borrow();
            (st.pending_html_reveal, st.rendered_message.is_some() && reading_stack_for_reveal.visible_child_name().as_deref() == Some("empty"))
        };
        // `stuck_on_empty` is a backstop for a scenario the selection
        // handler's same-message guard is meant to prevent, not the normal
        // path: `pending_html_reveal` disarmed by something other than a
        // genuine navigation to a different message, leaving this load's
        // `Finished` with nowhere to reveal to. If the pane is still on
        // "empty" and `rendered_message` (set by `render_body` right before
        // this load was issued) hasn't been overwritten by a newer render,
        // this Finished event is still the one worth revealing - the
        // alternative is a permanently blank pane, since WebKit only fires
        // `Finished` once per load.
        if armed || stuck_on_empty {
            tracing::debug!(armed, stuck_on_empty, "WebKit load finished; revealing reading pane");
            state_for_reveal.borrow_mut().pending_html_reveal = false;
            reveal_message_page(&reading_stack_for_reveal, &content_stack_for_reveal, "html");
        }
    });
    // Floor so the reading pane never gets squeezed down to something
    // unusably short if the window itself is resized very short - since
    // both Paneds here are horizontal, every pane always spans the full
    // window height, so this is really a minimum-window-height constraint
    // expressed on the pane that most needs it. Set directly on the Stack
    // (not on `text_scroller`'s child) because `Gtk.ScrolledWindow`
    // deliberately absorbs its child's size request instead of propagating
    // it - setting it here, on the Stack itself, is what actually reaches
    // the window's own size negotiation.
    reading_stack.set_size_request(-1, 300);

    let reading_pane_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    reading_pane_box.append(&reading_stack);

    let reading_card = card_section(&reading_pane_box);
    // See `message_card` above - the other half of the halved gap.
    reading_card.set_margin_start(0);

    // Keep the reading pane's card fully transparent while it's showing the
    // "no message selected" placeholder, so the window background image
    // shows straight through with no card tint - matching `.folder-pane`'s
    // translucency, but taken all the way to zero alpha since there's no
    // content to read against here. Driven off the stack's own signal
    // (rather than each `set_visible_child_name` call site) so every path
    // that flips back to "empty" - initial state above, `render_body`'s
    // fallback, and the reset on account disconnect - is covered for free.
    let update_reading_card_transparency = {
        let reading_card = reading_card.clone();
        move |stack: &gtk::Stack| {
            if stack.visible_child_name().as_deref() == Some("empty") {
                reading_card.add_css_class("reading-pane-transparent");
            } else {
                reading_card.remove_css_class("reading-pane-transparent");
            }
        }
    };
    update_reading_card_transparency(&reading_stack);
    reading_stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
        update_reading_card_transparency(stack);
    });

    // --- Resizable panes: folders | (messages | reading), each its own
    // rounded card. `resize_start_child(false)` keeps the sidebar-like
    // panes from silently growing when the window resizes; the reading
    // pane absorbs the extra space.
    let messages_reading_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&message_card)
        .end_child(&reading_card)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(368)
        .build();
    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&folder_card)
        .end_child(&messages_reading_paned)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(220)
        .build();
    main_paned.add_css_class("seamless-paned");

    let status_page_as_widget: gtk::Widget = status_page.clone().upcast();
    let main_paned_as_widget: gtk::Widget = main_paned.clone().upcast();
    let root_stack = gtk::Stack::new();
    // GtkStack defaults to homogeneous sizing on both axes, so its overall
    // requested size tracks the largest natural size across *every* named
    // page, not just the visible one - meaning a hidden page's content
    // filling in (e.g. the calendar/contacts sidebars populating from a
    // background sync) reflows the visible page too, and everything beside
    // it (nav_rail included).
    root_stack.set_hhomogeneous(false);
    root_stack.set_vhomogeneous(false);
    root_stack.add_named(&status_page_as_widget, Some("empty"));
    root_stack.add_named(&main_paned_as_widget, Some("mail"));
    root_stack.set_visible_child_name("empty");

    let calendar_status_page = adw::StatusPage::builder()
        .icon_name("x-office-calendar-symbolic")
        .title("No Calendar Accounts")
        .description("Add an account with Calendar enabled in GNOME Online Accounts to see events here.")
        .build();
    let calendar_main = Rc::new(calendar_view::build_main());
    let calendar_sidebar = calendar_view::build_sidebar();
    let calendar_sidebar_card = card_section(&calendar_sidebar.root);
    calendar_sidebar_card.add_css_class("folder-pane");
    let calendar_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&calendar_sidebar_card)
        .end_child(&calendar_main.root)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(260)
        .build();
    calendar_paned.add_css_class("seamless-paned");
    root_stack.add_named(&calendar_status_page, Some("calendar-empty"));
    root_stack.add_named(&calendar_paned, Some("calendar"));

    // --- Tasks view: a single card holding the grouped task list. Tasks come
    // from the same CalDAV accounts as the calendar (a `VTODO` per resource),
    // so "no calendar accounts" shows a status page like the calendar's.
    let tasks_status_page = adw::StatusPage::builder()
        .icon_name("view-task-symbolic")
        .title("No Calendar Accounts")
        .description("Add an account with Calendar enabled in GNOME Online Accounts to see tasks here.")
        .build();
    let tasks_view = Rc::new(crate::tasks_view::build_tasks_view());
    let tasks_card = card_section(&tasks_view.root);
    tasks_card.add_css_class("folder-pane");
    root_stack.add_named(&tasks_status_page, Some("tasks-empty"));
    root_stack.add_named(&tasks_card, Some("tasks"));

    // --- Lookout view: the dashboard tab at the top of the nav rail. A
    // snapshot of every connected account - people most contacted, emails
    // by time of day, outstanding tasks, and upcoming events - fed from
    // the mail caches and the calendar state, so "no accounts" shows a
    // status page like the other modules' empty states.
    let lookout_status_page = adw::StatusPage::builder()
        .icon_name("view-grid-symbolic")
        .title("No Accounts Connected")
        .description("Connect an account in GNOME Online Accounts to see your dashboard here.")
        .build();
    let lookout_view = Rc::new(crate::lookout_view::build_lookout_view());
    let lookout_card = card_section(&lookout_view.root);
    lookout_card.add_css_class("folder-pane");
    root_stack.add_named(&lookout_status_page, Some("lookout-empty"));
    root_stack.add_named(&lookout_card, Some("lookout"));

    let contacts_status_page = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("No Contact Accounts")
        .description("Add an account with Contacts enabled in GNOME Online Accounts to see people here.")
        .build();

    let contacts_category_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    let contacts_category_scroller = gtk::ScrolledWindow::builder()
        .child(&contacts_category_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let contacts_left_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    contacts_left_box.set_margin_start(8);
    contacts_left_box.set_margin_end(8);
    contacts_left_box.set_margin_top(8);
    contacts_left_box.set_margin_bottom(8);
    contacts_left_box.append(&contacts_category_scroller);

    let contacts_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    let contacts_scroller = gtk::ScrolledWindow::builder()
        .child(&contacts_list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let contacts_right_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    contacts_right_box.set_margin_start(8);
    contacts_right_box.set_margin_end(8);
    contacts_right_box.set_margin_top(8);
    contacts_right_box.set_margin_bottom(8);
    contacts_right_box.append(&gtk::Label::builder().label("People").xalign(0.0).css_classes(["heading"]).build());
    contacts_right_box.append(&contacts_scroller);

    let contacts_left_card = card_section(&contacts_left_box);
    contacts_left_card.add_css_class("folder-pane");
    let contacts_right_card = card_section(&contacts_right_box);
    let contacts_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&contacts_left_card)
        .end_child(&contacts_right_card)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(320)
        .build();
    contacts_paned.add_css_class("seamless-paned");
    root_stack.add_named(&contacts_status_page, Some("contacts-empty"));
    root_stack.add_named(&contacts_paned, Some("contacts"));

    // Built here (rather than down by the rest of the Config wiring below)
    // so `config_view.paned` exists in time to join the shared pane-width
    // persistence closure right below.
    let config_view = Rc::new(crate::config_view::build());

    // The one real title bar for the window - owns the actual
    // minimize/maximize/close buttons. The per-card header bars inside
    // `root_stack` are explicitly told not to show these (see
    // `card_section`), so there's exactly one set, not four.
    let window_header = adw::HeaderBar::new();
    window_header.set_title_widget(Some(&adw::WindowTitle::new("Lookout", "")));
    // The Lookout tab lives in the header's top-left corner rather than the
    // nav rail (it's the app's own dashboard, and its Observatorium icon
    // reads like a logo), with the permanent mail-search entry to its
    // right. Still a toggle button in the rail's group, so the tab switch
    // stays mutually exclusive with the other views.
    let lookout_icon_image = nav_rail_image(
        "/io/github/gavindi/Lookout/icons/observatorium-1.svg",
        include_bytes!("../../../data/resources/icons/observatorium-1.svg"),
    );
    let lookout_view_button = gtk::ToggleButton::builder()
        .child(&lookout_icon_image)
        .css_classes(["flat"])
        .tooltip_text("Lookout")
        .build();
    window_header.pack_start(&lookout_view_button);
    // The old 62px margin existed to line the search up with the menu bar
    // below (which the rail shifts right); the header icon now owns that
    // corner, so the search sits directly beside it.
    search_entry.set_margin_start(12);
    window_header.pack_start(&search_entry);
    // A second home for the View tab's Calendar overview toggle, docked at
    // the header's right end just left of the (debug-only) .eml opener.
    // Wired to `overview_pane_toggle` below, so either button flips both -
    // they can never disagree about the pane's visibility, and the toggle
    // state survives a round-trip through the Calendar/Config views. Like
    // Home/View, it goes honest-disabled while another module is active
    // (see the nav-rail handlers).
    let header_calendar_overview_toggle = gtk::ToggleButton::builder().icon_name("x-office-calendar-symbolic").css_classes(["flat"]).build();
    header_calendar_overview_toggle.set_tooltip_text(Some("Calendar overview"));
    window_header.pack_end(&header_calendar_overview_toggle);
    #[cfg(debug_assertions)]
    window_header.pack_end(&open_eml_button);

    // --- Menu bar row (File/Home/View/Help). File (Quit) and Help (About)
    // are `MenuButton`s with a real popover; Home/View are the ribbon's tab
    // strip - mutually-exclusive toggle buttons that switch the ribbon
    // content row below (see `view_toolbar_stack`). Home holds the command
    // toolbar; View holds the pane-visibility layout toggles. Mail-only:
    // they're disabled while a Calendar/Config module is active (see the
    // nav-rail handlers), matching the codebase's honest-disabled convention.
    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    let help_menu = gio::Menu::new();
    help_menu.append(Some("About Lookout"), Some("app.about"));

    let file_button = gtk::MenuButton::builder().label("File").css_classes(["flat"]).menu_model(&file_menu).build();
    let home_button = gtk::ToggleButton::builder().label("Home").css_classes(["flat", "ribbon-tab"]).active(true).build();
    let view_button = gtk::ToggleButton::builder().label("View").css_classes(["flat", "ribbon-tab"]).build();
    view_button.set_group(Some(&home_button));
    let help_button = gtk::MenuButton::builder().label("Help").css_classes(["flat"]).menu_model(&help_menu).build();

    // Which ribbon tab is active ("home" | "view") and which nav-rail module
    // is selected ("mail" | "calendar" | "config") - together they decide the
    // `view_toolbar_stack`'s visible child (see `ribbon_stack_name`). The tab
    // state persists across module switches; the nav-rail handlers re-enable
    // Home/View on Mail and disable them elsewhere.
    let active_ribbon_tab: Rc<Cell<&'static str>> = Rc::new(Cell::new("home"));
    let current_module: Rc<Cell<&'static str>> = Rc::new(Cell::new("mail"));

    let menu_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).css_classes(["toolbar"]).build();
    menu_bar.append(&file_button);
    menu_bar.append(&home_button);
    menu_bar.append(&view_button);
    menu_bar.append(&help_button);

    // --- Command toolbar row. `compose_button`, `reply_button`,
    // `reply_all_button`, `forward_button`, `delete_button`,
    // `archive_button`, `report_button`, `star_button`, `snooze_button`, and
    // `more_button` (a menu of reading-pane extras like "Save as .eml…") are
    // backed by real functionality.
    let reply_button = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
    reply_button.set_tooltip_text(Some("Reply"));
    let reply_all_button = gtk::Button::from_icon_name("mail-reply-all-symbolic");
    reply_all_button.set_tooltip_text(Some("Reply All"));
    let forward_button = gtk::Button::from_icon_name("mail-forward-symbolic");
    forward_button.set_tooltip_text(Some("Forward"));
    let delete_button = gtk::Button::from_icon_name("user-trash-symbolic");
    delete_button.set_tooltip_text(Some("Delete"));
    let archive_button = gtk::Button::from_icon_name("mail-archive-symbolic");
    archive_button.set_tooltip_text(Some("Archive"));
    let report_button = gtk::Button::from_icon_name("mail-mark-junk-symbolic");
    report_button.set_tooltip_text(Some("Report"));
    let star_button = gtk::Button::from_icon_name("mail-mark-important-symbolic");
    star_button.set_tooltip_text(Some("Star/Unstar"));
    refresh_star_button(&star_button, &message_list);
    // Add as Task: opens the task editor prefilled from the selected
    // message (subject as title, sender/date in Notes - `CalendarTask` has
    // no url field). Distinct from Star/Unstar above, which only toggles the
    // IMAP `\Flagged` bit. Its icon starts outline and is kept in sync with
    // whether the selected message already has an associated task by
    // `refresh_task_button`, registered once `calendar_state` exists below.
    let task_button = gtk::Button::from_icon_name(themed_icon_name(&["flag-outline-thin-symbolic", "flag-outline-symbolic", "mail-mark-important-symbolic"]));
    task_button.set_tooltip_text(Some("Follow-up"));
    // Mark read/unread: no explicit toolbar action for this existed before -
    // only the implicit mark-as-read that opening a message already does.
    // Same aggregate-direction policy as Star/Unstar: any unread message in
    // the selection means the action marks everything read; only when every
    // selected message is already read does it become "mark all unread."
    let mark_read_button = gtk::Button::from_icon_name(themed_icon_name(&["mail-mark-unread-symbolic", "mail-unread-symbolic", "emblem-ok-symbolic"]));
    refresh_mark_read_button(&mark_read_button, &message_list);
    // Categorize: a menu of the defined color tags, toggle-checked against
    // the selected message. Its popover is rebuilt on every `show` so the
    // check states track whichever message is selected when it opens.
    let categorize_button = gtk::MenuButton::builder()
        .icon_name(themed_icon_name(&["tag-symbolic", "mail-mark-important-symbolic"]))
        .build();
    categorize_button.set_tooltip_text(Some("Categorize"));
    let categorize_popover = gtk::Popover::new();
    categorize_button.set_popover(Some(&categorize_popover));
    {
        let tags = tags.clone();
        let state = state.clone();
        let message_list = message_list.clone();
        let tag_colors = tag_colors.clone();
        categorize_popover.connect_show(move |popover| {
            let target = message_list.selected_summary();
            let boxed = build_tag_menu(&tags, &state, target, &message_list, &tag_colors);
            popover.set_child(Some(&boxed));
        });
    }
    let snooze_button = gtk::Button::from_icon_name("appointment-soon-symbolic");
    snooze_button.set_tooltip_text(Some("Snooze"));
    // "More": a popover of reading-pane extras (currently "Save as .eml…").
    // The popover's contents are rebuilt on every `show`, like
    // `categorize_button`'s, so the enabled state can track whichever
    // message is selected when it opens.
    let more_button = gtk::MenuButton::builder().icon_name("view-more-symbolic").build();
    more_button.set_tooltip_text(Some("More"));
    let more_popover = gtk::Popover::new();
    more_button.set_popover(Some(&more_popover));
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let reading_stack_for_menu = reading_stack.clone();
        let more_popover_for_menu = more_popover.clone();
        more_popover.connect_show(move |popover| {
            let has_selection = message_list.selected_summary().is_some();
            let menu = build_more_menu(has_selection, &message_list, &state, &reading_stack_for_menu, &more_popover_for_menu);
            popover.set_child(Some(&menu));
        });
    }

    let command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    command_toolbar.append(&compose_button);
    command_toolbar.append(&reply_button);
    command_toolbar.append(&reply_all_button);
    command_toolbar.append(&forward_button);
    command_toolbar.append(&delete_button);
    command_toolbar.append(&archive_button);
    command_toolbar.append(&report_button);
    command_toolbar.append(&star_button);
    command_toolbar.append(&task_button);
    command_toolbar.append(&mark_read_button);
    command_toolbar.append(&categorize_button);
    command_toolbar.append(&snooze_button);
    command_toolbar.append(&more_button);

    // --- Calendar's own command toolbar row, swapped in for the Mail one
    // (see `view_toolbar_stack` below) when the Calendar nav-rail button is
    // active. New Event opens the event editor (wired later, once
    // `calendar_state` exists); all five segmented options (Day/Work
    // week/Week/Month/Split) switch the main panel's stack; Filter/Share/Print
    // remain disabled placeholders.
    let new_event_button = gtk::Button::from_icon_name("appointment-new-symbolic");
    new_event_button.set_tooltip_text(Some("New Event"));

    let day_view_button = gtk::ToggleButton::builder().label("Day").build();
    let work_week_view_button = gtk::ToggleButton::builder().label("Work week").build();
    let week_view_button = gtk::ToggleButton::builder().label("Week").build();
    let month_view_button = gtk::ToggleButton::builder().label("Month").active(true).build();
    let split_view_button = gtk::ToggleButton::builder().label("Split view").build();
    // One mutual-exclusion group so exactly one view is active at a time
    // (same `set_group` trick as the nav rail's Mail/Calendar buttons).
    split_view_button.set_group(Some(&month_view_button));
    week_view_button.set_group(Some(&split_view_button));
    work_week_view_button.set_group(Some(&week_view_button));
    day_view_button.set_group(Some(&work_week_view_button));
    let view_switch_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).css_classes(["linked"]).build();
    view_switch_box.append(&day_view_button);
    view_switch_box.append(&work_week_view_button);
    view_switch_box.append(&week_view_button);
    view_switch_box.append(&month_view_button);
    view_switch_box.append(&split_view_button);
    for (button, view) in [
        (&day_view_button, "day"),
        (&work_week_view_button, "workweek"),
        (&week_view_button, "week"),
        (&month_view_button, "month"),
        (&split_view_button, "split"),
    ] {
        let calendar_main = calendar_main.clone();
        button.connect_toggled(move |btn| {
            if btn.is_active() {
                calendar_view::set_view(&calendar_main, view);
            }
        });
    }

    let filter_button = gtk::Button::from_icon_name("edit-find-symbolic");
    filter_button.set_tooltip_text(Some("Filter"));
    filter_button.set_sensitive(false);
    let share_button = gtk::Button::from_icon_name("send-to-symbolic");
    share_button.set_tooltip_text(Some("Share"));
    share_button.set_sensitive(false);
    let print_button = gtk::Button::from_icon_name("printer-symbolic");
    print_button.set_tooltip_text(Some("Print"));
    print_button.set_sensitive(false);

    let calendar_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    calendar_command_toolbar.append(&new_event_button);
    calendar_command_toolbar.append(&view_switch_box);
    calendar_command_toolbar.append(&filter_button);
    calendar_command_toolbar.append(&share_button);
    calendar_command_toolbar.append(&print_button);

    // --- Tasks' own command toolbar row, swapped in when the Tasks nav-rail
    // button is active. New task opens the task editor (wired later, once
    // `calendar_state` exists); Connect Google Tasks runs the interactive
    // OAuth flow for the account's Google GOA account.
    let new_task_button = gtk::Button::from_icon_name(themed_icon_name(&["view-task-symbolic", "appointment-soon-symbolic"]));
    new_task_button.set_tooltip_text(Some("New Task"));
    let connect_google_tasks_button = gtk::Button::with_label("Connect Google Tasks");
    connect_google_tasks_button.set_tooltip_text(Some("Sign in to Google Tasks (the Tasks API, separate from Google's event-only CalDAV)"));

    let tasks_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    tasks_command_toolbar.append(&new_task_button);
    tasks_command_toolbar.append(&connect_google_tasks_button);

    // --- View tab's ribbon content (Mail module): a "Layout" group of
    // pane-visibility toggles - Folder pane / Reading pane / Calendar
    // overview. All three default on; their click handlers live in a later
    // block (after every pane widget exists), write through to GSettings
    // (see `settings`), and are re-applied from there at startup.
    let layout_label = gtk::Label::builder().label("Layout").css_classes(["ribbon-group-label", "dim-label"]).build();
    let folder_pane_toggle = gtk::ToggleButton::builder().icon_name("folder-symbolic").build();
    folder_pane_toggle.set_tooltip_text(Some("Folder pane"));
    folder_pane_toggle.set_active(true);
    let reading_pane_toggle = gtk::ToggleButton::builder().icon_name("document-preview-symbolic").build();
    reading_pane_toggle.set_tooltip_text(Some("Reading pane"));
    reading_pane_toggle.set_active(true);
    let overview_pane_toggle = gtk::ToggleButton::builder().icon_name("x-office-calendar-symbolic").build();
    overview_pane_toggle.set_tooltip_text(Some("Calendar overview"));
    overview_pane_toggle.set_active(true);
    let conversations_toggle = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["mail-replied-symbolic", "mail-forwarded-symbolic"]))
        .build();
    conversations_toggle.set_tooltip_text(Some("Conversations: group replies under collapsible thread headers"));
    conversations_toggle.set_active(true);
    let view_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    view_toolbar.append(&layout_label);
    view_toolbar.append(&folder_pane_toggle);
    view_toolbar.append(&reading_pane_toggle);
    view_toolbar.append(&overview_pane_toggle);
    view_toolbar.append(&conversations_toggle);

    let view_toolbar_stack = gtk::Stack::new();
    view_toolbar_stack.add_named(&command_toolbar, Some("mail-home"));
    view_toolbar_stack.add_named(&view_toolbar, Some("mail-view"));
    view_toolbar_stack.add_named(&calendar_command_toolbar, Some("calendar"));
    view_toolbar_stack.add_named(&tasks_command_toolbar, Some("tasks"));

    let contacts_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    let contacts_toolbar_label = gtk::Label::builder().label("People").xalign(0.0).css_classes(["dim-label"]).build();
    contacts_command_toolbar.append(&contacts_toolbar_label);
    let new_contact_button = gtk::Button::with_label("New contact");
    contacts_command_toolbar.append(&new_contact_button);
    let import_contacts_button = gtk::Button::with_label("Import…");
    contacts_command_toolbar.append(&import_contacts_button);
    let export_contacts_button = gtk::Button::with_label("Export…");
    contacts_command_toolbar.append(&export_contacts_button);
    let manage_groups_button = gtk::Button::with_label("Manage groups…");
    contacts_command_toolbar.append(&manage_groups_button);
    let open_contacts_window_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popout1", "window-new-symbolic", "view-restore-symbolic"]));
    open_contacts_window_button.set_tooltip_text(Some("Open the People screen in its own window"));
    contacts_command_toolbar.append(&open_contacts_window_button);
    view_toolbar_stack.add_named(&contacts_command_toolbar, Some("contacts"));

    // The Lookout dashboard's command toolbar: a single Refresh button that
    // re-reads the caches and widens the calendar sync horizon so upcoming
    // events reach into next month.
    let lookout_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let lookout_refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
    lookout_refresh_button.set_tooltip_text(Some("Refresh dashboard"));
    lookout_command_toolbar.append(&lookout_refresh_button);
    view_toolbar_stack.add_named(&lookout_command_toolbar, Some("lookout"));

    // --- View-switcher rail: a narrow, deliberately unstyled (no `.card`,
    // no background) strip along the window's left edge so the background
    // image shows straight through it. Five views today (Mail/Calendar/
    // People/Tasks/Config - the Lookout tab lives in the header instead),
    // joined into one toggle group for mutual-exclusive selection.
    let mail_icon_image = nav_rail_image("/io/github/gavindi/Lookout/icons/email-1.svg", include_bytes!("../../../data/resources/icons/email-1.svg"));
    let mail_view_button = gtk::ToggleButton::builder()
        .child(&mail_icon_image)
        .css_classes(["flat"])
        .tooltip_text("Mail")
        .active(true)
        .build();
    let calendar_icon_image = nav_rail_image(
        "/io/github/gavindi/Lookout/icons/calendar-1.svg",
        include_bytes!("../../../data/resources/icons/calendar-1.svg"),
    );
    let calendar_view_button = gtk::ToggleButton::builder()
        .child(&calendar_icon_image)
        .css_classes(["flat"])
        .tooltip_text("Calendar")
        .build();
    calendar_view_button.set_group(Some(&mail_view_button));
    let contacts_icon_image = nav_rail_image(
        "/io/github/gavindi/Lookout/icons/contact-1.svg",
        include_bytes!("../../../data/resources/icons/contact-1.svg"),
    );
    let contacts_view_button = gtk::ToggleButton::builder()
        .child(&contacts_icon_image)
        .css_classes(["flat"])
        .tooltip_text("People")
        .build();
    contacts_view_button.set_group(Some(&calendar_view_button));
    let tasks_icon_image = nav_rail_image("/io/github/gavindi/Lookout/icons/task-1.svg", include_bytes!("../../../data/resources/icons/task-1.svg"));
    let tasks_view_button = gtk::ToggleButton::builder().child(&tasks_icon_image).css_classes(["flat"]).tooltip_text("Tasks").build();
    tasks_view_button.set_group(Some(&contacts_view_button));
    // The Lookout button was built up in the header section; joining it to
    // the rail buttons' toggle group keeps the tab switching exclusive.
    lookout_view_button.set_group(Some(&mail_view_button));

    // `vexpand(true)` so the rail stretches the window's full height (it
    // sits beside `outer_toolbar_view` - header bar, menu bar, and command
    // toolbar included - rather than below those top bars). The content is a
    // fixed-height Box, wrapped in a scrolled window: a window shrunk shorter
    // than the buttons' total height would otherwise clip the bottom-anchored
    // Config button off-screen (a Box clips its trailing children first), and
    // scrolling keeps every rail button reachable at any height.
    let nav_rail_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(56)
        .margin_top(6)
        .margin_start(6)
        .spacing(6)
        .vexpand(true)
        .build();
    nav_rail_content.append(&mail_view_button);
    nav_rail_content.append(&calendar_view_button);
    nav_rail_content.append(&contacts_view_button);
    nav_rail_content.append(&tasks_view_button);
    let nav_rail = gtk::ScrolledWindow::builder()
        .child(&nav_rail_content)
        .has_frame(false)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .overlay_scrolling(true)
        .vexpand(true)
        .build();

    // --- Mail-screen calendar overview pane: a mini month-picker + a list
    // of the clicked day's events and every outstanding task, docked to the
    // far right of the window, spanning the same full height as `nav_rail`
    // (it's a sibling in `window_body`, not nested inside `root_stack`).
    // Mail-only - the Calendar view already has its own full sidebar with a
    // mini-calendar.
    let mail_calendar_overview = calendar_view::build_mini();
    // Half-width day cells (see `.mini-calendar-compact` in
    // `install_calendar_css`). The day buttons' own natural size is what set
    // the pane's real width - the `width_request` below is only a floor - so
    // narrowing the pane means narrowing the buttons.
    mail_calendar_overview.root.add_css_class("mini-calendar-compact");
    let mail_overview_day_list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(8).build();
    // Matches `build_sidebar()`'s own width_request - without an explicit
    // cap here, the mini-calendar's day-button grid requests its natural
    // (much wider) size instead of a compact peek-pane width.
    let mail_overview_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(140)
        .margin_top(2)
        .margin_bottom(2)
        .margin_start(2)
        .margin_end(2)
        .build();
    mail_overview_box.append(&mail_calendar_overview.root);
    mail_overview_box.append(&mail_overview_day_list);

    let mail_calendar_overview_card = card_section(&mail_overview_box);
    mail_calendar_overview_card.add_css_class("folder-pane");
    mail_calendar_overview_card.set_vexpand(true);
    // True while the window is too narrow to show the whole overview pane:
    // the pane is auto-hidden then instead of letting the paned clip it, and
    // stays hidden until a resize makes it fit again (the toggle handler and
    // the mail-tab handler both consult this flag; see `check_overview_fits`).
    let overview_forced_hidden: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // --- Ribbon tab strip + View-tab pane toggles. Home/View swap the
    // ribbon content row (`view_toolbar_stack`) between Mail's command
    // toolbar and the Layout toggles; the three toggles show/hide their
    // pane. Home/View only make sense on the Mail module - the Calendar/
    // Config nav handlers disable them below, so the tab buttons can't be
    // clicked there (and the `if btn.is_active()` guard makes re-entrant
    // toggles no-ops anyway).
    {
        let view_toolbar_stack = view_toolbar_stack.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        home_button.connect_toggled(move |btn| {
            if btn.is_active() {
                active_ribbon_tab.set("home");
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name(current_module.get(), "home"));
            }
        });
    }
    {
        let view_toolbar_stack = view_toolbar_stack.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                active_ribbon_tab.set("view");
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name(current_module.get(), "view"));
            }
        });
    }
    {
        let folder_card = folder_card.clone();
        let state = state.clone();
        folder_pane_toggle.connect_toggled(move |btn| {
            state.borrow().settings.set_bool(crate::settings::LAYOUT_FOLDER_PANE, btn.is_active());
            folder_card.set_visible(btn.is_active());
        });
    }
    {
        let reading_card = reading_card.clone();
        let state = state.clone();
        reading_pane_toggle.connect_toggled(move |btn| {
            state.borrow().settings.set_bool(crate::settings::LAYOUT_READING_PANE, btn.is_active());
            reading_card.set_visible(btn.is_active());
        });
    }
    {
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let overview_forced_hidden = overview_forced_hidden.clone();
        let state = state.clone();
        let header_calendar_overview_toggle = header_calendar_overview_toggle.clone();
        overview_pane_toggle.connect_toggled(move |btn| {
            state.borrow().settings.set_bool(crate::settings::LAYOUT_CALENDAR_OVERVIEW, btn.is_active());
            // The window-width check may have auto-hidden the pane while the
            // window is too narrow; don't override that here.
            mail_calendar_overview_card.set_visible(btn.is_active() && !overview_forced_hidden.get());
            // Keep the header's copy in lockstep. `set_active` with the
            // button's current value is a no-op, so the mirror handler below
            // can't loop back into this one.
            header_calendar_overview_toggle.set_active(btn.is_active());
        });
    }
    {
        // The header's own copy is a mirror: flipping it just drives
        // `overview_pane_toggle`, whose handler owns the setting, the pane's
        // visibility, and this button's state.
        let overview_pane_toggle = overview_pane_toggle.clone();
        header_calendar_overview_toggle.connect_toggled(move |btn| {
            overview_pane_toggle.set_active(btn.is_active());
        });
    }
    {
        // Conversations: re-render the message list in or out of thread
        // grouping. The mode lives in the model (`set_threaded`), and the
        // setting persists it - mirroring how the sort controls own their
        // state in `UiState` while GSettings mirrors it.
        let state = state.clone();
        let message_list = message_list.clone();
        conversations_toggle.connect_toggled(move |btn| {
            state.borrow().settings.set_bool(crate::settings::MAIL_THREADED, btn.is_active());
            message_list.set_threaded(btn.is_active());
        });
    }
    // Phase 5: apply the persisted Layout toggles now that their handlers are
    // wired (each `set_active` fires `toggled`, which sets the card's
    // visibility), so the pane layout comes back as the user left it. The
    // handlers' write-through makes these calls no-ops on restart.
    {
        let persisted = state.borrow().settings.clone();
        folder_pane_toggle.set_active(persisted.get_bool(crate::settings::LAYOUT_FOLDER_PANE));
        reading_pane_toggle.set_active(persisted.get_bool(crate::settings::LAYOUT_READING_PANE));
        overview_pane_toggle.set_active(persisted.get_bool(crate::settings::LAYOUT_CALENDAR_OVERVIEW));
        conversations_toggle.set_active(persisted.get_bool(crate::settings::MAIL_THREADED));
    }

    // Which sub-page each view should show when its nav-rail button becomes
    // active - kept up to date by the discovery/event handlers below (which
    // only actually flip `root_stack`'s visible child if their own button is
    // the one currently active, so a background sync on the other view
    // never yanks the screen out from under whichever one the user is
    // looking at).
    let current_mail_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("empty"));
    let current_calendar_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("calendar-empty"));
    let current_contacts_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("contacts-empty"));
    let current_tasks_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("tasks-empty"));
    let current_lookout_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("lookout-empty"));
    // The standalone People window, when the People screen is popped out of
    // the main window (see the "Open in new window" contacts-toolbar button).
    // `None` while the paned lives in `root_stack`; `Some` while it lives in
    // its own window, with a placeholder occupying the stack's "contacts"
    // slot. The window is created on demand - never at startup - and closed
    // by re-attaching the paned to the main window. The placeholder starts
    // unparented: it only ever enters `root_stack` on detach (under the
    // "contacts" name) and leaves it again on re-attach, so it can never be
    // added while already parented (which GTK asserts against and which
    // otherwise leaves a duplicate "contacts" child shadowing the paned).
    let contacts_window: Rc<RefCell<Option<adw::Window>>> = Rc::new(RefCell::new(None));
    let contacts_detached_page = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("People are in a separate window")
        .description("Close that window to bring them back here.")
        .build();
    {
        let root_stack = root_stack.clone();
        let current_mail_page = current_mail_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        let overview_pane_toggle = overview_pane_toggle.clone();
        let overview_forced_hidden = overview_forced_hidden.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        let header_calendar_overview_toggle = header_calendar_overview_toggle.clone();
        mail_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("mail");
                root_stack.set_visible_child_name(current_mail_page.get());
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name("mail", active_ribbon_tab.get()));
                // Respect the View tab's toggle rather than forcing the
                // overview pane back on after a Calendar/Config round-trip -
                // and the width-based auto-hide while the window is narrow.
                mail_calendar_overview_card.set_visible(overview_pane_toggle.is_active() && !overview_forced_hidden.get());
                home_button.set_sensitive(true);
                view_button.set_sensitive(true);
                header_calendar_overview_toggle.set_sensitive(true);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_calendar_page = current_calendar_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        let header_calendar_overview_toggle = header_calendar_overview_toggle.clone();
        calendar_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("calendar");
                root_stack.set_visible_child_name(current_calendar_page.get());
                view_toolbar_stack.set_visible_child_name("calendar");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
                header_calendar_overview_toggle.set_sensitive(false);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_contacts_page = current_contacts_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        let contacts_window = contacts_window.clone();
        let header_calendar_overview_toggle = header_calendar_overview_toggle.clone();
        contacts_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("contacts");
                root_stack.set_visible_child_name(current_contacts_page.get());
                view_toolbar_stack.set_visible_child_name("contacts");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
                header_calendar_overview_toggle.set_sensitive(false);
                // People may have been popped out into their own window -
                // bring that to the front instead of the in-stack placeholder.
                if let Some(win) = contacts_window.borrow().as_ref() {
                    win.present();
                }
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_tasks_page = current_tasks_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        let header_calendar_overview_toggle = header_calendar_overview_toggle.clone();
        tasks_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("tasks");
                root_stack.set_visible_child_name(current_tasks_page.get());
                view_toolbar_stack.set_visible_child_name("tasks");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
                header_calendar_overview_toggle.set_sensitive(false);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_lookout_page = current_lookout_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        let header_calendar_overview_toggle = header_calendar_overview_toggle.clone();
        lookout_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("lookout");
                root_stack.set_visible_child_name(current_lookout_page.get());
                view_toolbar_stack.set_visible_child_name("lookout");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
                header_calendar_overview_toggle.set_sensitive(false);
            }
        });
    }

    // --- Pop-out People window: "Open in new window" moves the contacts
    // paned out of `root_stack` into a standalone `adw::Window` (the app's
    // first multi-window surface); closing that window re-attaches the paned
    // to the main window, preserving every widget's state. The window gets
    // its own header bar (title "People") so it can be dragged, with a "Back
    // to main window" button that closes it - and the standard close button
    // does the same, both via the `close-request` handler below. While
    // detached, a placeholder occupies the stack's "contacts" slot so the
    // page name stays valid for every other code path (rail handler,
    // discovery `show_page`). Only one instance can exist; re-clicking the
    // button just re-presents it. ---
    {
        let contacts_paned = contacts_paned.clone();
        let root_stack = root_stack.clone();
        let contacts_detached_page = contacts_detached_page.clone();
        let current_module = current_module.clone();
        let current_contacts_page = current_contacts_page.clone();
        let contacts_view_button = contacts_view_button.clone();
        let contacts_window = contacts_window.clone();
        open_contacts_window_button.connect_clicked(move |_| {
            if let Some(existing) = contacts_window.borrow().as_ref() {
                existing.present();
                return;
            }
            let win = adw::Window::builder().default_width(960).default_height(680).build();
            let header = adw::HeaderBar::new();
            header.set_title_widget(Some(&adw::WindowTitle::new("People", "")));
            let back_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popin1", "go-previous-symbolic", "go-back-symbolic"]));
            back_button.set_tooltip_text(Some("Return the People screen to the main window"));
            header.pack_start(&back_button);
            {
                let win = win.clone();
                back_button.connect_clicked(move |_| win.close());
            }
            let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
            content_box.append(&header);
            {
                let contacts_paned = contacts_paned.clone();
                let root_stack = root_stack.clone();
                let contacts_detached_page = contacts_detached_page.clone();
                let current_module = current_module.clone();
                let current_contacts_page = current_contacts_page.clone();
                let contacts_view_button = contacts_view_button.clone();
                let contacts_window = contacts_window.clone();
                win.connect_close_request(move |_| {
                    // Move the paned back into the main window's stack. The
                    // placeholder is removed by widget, not by name - it was
                    // added under "contacts" on detach so the page name
                    // stays valid while detached, and it must not linger as
                    // a second "contacts" child shadowing the paned.
                    if let Some(parent) = contacts_paned.parent().and_then(|parent| parent.downcast::<gtk::Box>().ok()) {
                        parent.remove(&contacts_paned);
                    }
                    root_stack.remove(&contacts_detached_page);
                    root_stack.add_named(&contacts_paned, Some("contacts"));
                    if current_module.get() == "contacts" && contacts_view_button.is_active() {
                        root_stack.set_visible_child_name(current_contacts_page.get());
                    }
                    *contacts_window.borrow_mut() = None;
                    glib::Propagation::Proceed
                });
            }
            // Move the paned out of the main stack into the new window, and
            // put the placeholder into its old slot. The placeholder must be
            // unparented first (it never lives in the stack while the paned
            // is attached) - `add_named` on an already-parented widget trips
            // GTK's `gtk_widget_set_parent` assertion.
            root_stack.remove(&contacts_paned);
            root_stack.remove(&contacts_detached_page);
            root_stack.add_named(&contacts_detached_page, Some("contacts"));
            if current_module.get() == "contacts" {
                root_stack.set_visible_child_name("contacts");
            }
            content_box.append(&contacts_paned);
            contacts_paned.set_vexpand(true);
            win.set_content(Some(&content_box));
            *contacts_window.borrow_mut() = Some(win.clone());
            win.present();
        });
    }

    // --- Full-text search wiring: the entry lives permanently in the header
    // bar, and a query started from *any* module switches to Mail (activating
    // its nav button re-runs the module handler, which re-shows the mail
    // page and re-enables Home/View) so the results are visible. The entry
    // itself is built above, next to the state it searches. ---
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        let search_debounce = search_debounce.clone();
        let mail_view_button = mail_view_button.clone();
        search_entry.connect_search_changed(move |entry| {
            let query = entry.text();
            if query.trim().is_empty() {
                // Clearing the field (its X, or a programmatic `set_text("")`
                // from `exit_search`) ends the search right away - no debounce
                // needed. Bumping the token also invalidates any timeout armed
                // for a query still being typed.
                search_debounce.set(search_debounce.get() + 1);
                exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, entry);
                return;
            }
            let token = search_debounce.get() + 1;
            search_debounce.set(token);
            // Clone everything the timeout needs: the outer closure is `Fn`
            // and fires again on the next keystroke, so the timeout's own
            // `move` closure can't borrow from it.
            let state = state.clone();
            let worker = worker.clone();
            let message_list = message_list.clone();
            let list_header = list_header.clone();
            let search_debounce = search_debounce.clone();
            let mail_view_button = mail_view_button.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS as u64), move || {
                if search_debounce.get() != token {
                    return;
                }
                start_search(&state, &worker, &message_list, &list_header, query.as_str());
                // Land the results on screen: if the user started typing from
                // another module, activate Mail (no-op if already active).
                mail_view_button.set_active(true);
            });
        });
    }
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        let search_debounce = search_debounce.clone();
        // Esc in the entry: leave search mode entirely rather than just
        // clearing the field (`GtkSearchEntry` emits `stop-search` on Esc).
        search_entry.connect_stop_search(move |entry| {
            if search_debounce.get() != 0 {
                // Bump the debounce token so a timeout armed for the query
                // being typed is invalidated before we clear the entry - the
                // `set_text("")` below would otherwise fire `search-changed`
                // and re-enter a search for the (already-cleared) query.
                search_debounce.set(search_debounce.get() + 1);
            }
            exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, entry);
        });
    }

    // The nav rail runs the full height *below the title bar* - it sits
    // beside the menu bar/command toolbar/mail-or-calendar content, but not
    // beside `window_header` itself, which stays the one real title bar
    // spanning the full window width at the very top.
    root_stack.set_hexpand(true);
    root_stack.set_vexpand(true);

    // The icon command toolbar (Mail's/Calendar's) gets its own dark grey
    // subgroup, distinct from the menu bar's black background.
    let icon_toolbar_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["window-icon-toolbar-background"])
        .overflow(gtk::Overflow::Hidden)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    icon_toolbar_box.append(&view_toolbar_stack);

    // Menu bar + icon toolbar grouped onto their own shared black
    // background, rather than each row painting (or not painting) its own.
    // Rounded like the icon toolbar's grey subgroup, and margined so the
    // corners show against the window background image.
    let toolbars_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["window-toolbars-background"])
        .overflow(gtk::Overflow::Hidden)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    toolbars_box.append(&menu_bar);
    toolbars_box.append(&icon_toolbar_box);

    let inner_content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).vexpand(true).build();
    inner_content_box.append(&toolbars_box);
    inner_content_box.append(&root_stack);

    // Resizable split between the main content and the overview pane -
    // `nav_rail` stays a fixed-width sibling outside the split (it isn't
    // meant to be resizable), but the overview pane's width is user-
    // draggable like every other pane split in this app.
    let content_and_overview_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&inner_content_box)
        .end_child(&mail_calendar_overview_card)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(1340)
        .build();
    content_and_overview_paned.add_css_class("seamless-paned");

    // If the window gets too narrow to fit the overview pane's whole minimum
    // width beside the content's minimum, GTK clips the pane's right edge
    // instead of shrinking it (both paned children refuse to shrink). Hide
    // the pane in that case, and bring it back - if the View tab's toggle is
    // still on - once a resize widens the window enough again. The separator
    // also takes space between the children, so the fit threshold includes a
    // small cushion so the pane neither clips nor flickers exactly at the
    // boundary. `measure` ignores visibility, so the check works whether the
    // pane is currently shown or hidden.
    let check_overview_fits = {
        let content_and_overview_paned = content_and_overview_paned.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let overview_pane_toggle = overview_pane_toggle.clone();
        let overview_forced_hidden = overview_forced_hidden.clone();
        let current_module = current_module.clone();
        move || {
            // Not allocated yet (build time / before the first map): leave
            // visibility alone until the first real resize check.
            if content_and_overview_paned.width() <= 0 {
                return;
            }
            let start_min = content_and_overview_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
            let end_min = mail_calendar_overview_card.measure(gtk::Orientation::Horizontal, -1).0;
            let fits = content_and_overview_paned.width() >= start_min + end_min + 8;
            if !fits {
                overview_forced_hidden.set(true);
                mail_calendar_overview_card.set_visible(false);
            } else if overview_forced_hidden.replace(false) {
                // Only the Mail module shows the overview pane; the other
                // modules' handlers own hiding it there.
                if current_module.get() == "mail" {
                    mail_calendar_overview_card.set_visible(overview_pane_toggle.is_active());
                }
            }
        }
    };

    let window_body = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
    window_body.append(&nav_rail);
    window_body.append(&content_and_overview_paned);

    let outer_toolbar_view = adw::ToolbarView::new();
    outer_toolbar_view.add_top_bar(&window_header);
    outer_toolbar_view.set_content(Some(&window_body));

    toast_overlay.set_child(Some(&outer_toolbar_view));
    toast_overlay.set_hexpand(true);
    toast_overlay.set_vexpand(true);

    let window_overlay = gtk::Overlay::new();
    window_overlay.set_child(Some(&background));
    // The dimming layer between the background image and the app content:
    // an opaque-black widget whose opacity (1 - brightness) darkens a
    // user-picked background toward black per Config → Appearance →
    // "Background dimming". `can_target(false)` keeps it click-through.
    // Starts at 50% opacity - the bundled artwork's default brightness. A
    // custom background overrides this below with its own stored GSettings
    // value once we know one is in use.
    let background_dim = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    background_dim.set_can_target(false);
    background_dim.set_css_classes(&["window-background-dim"]);
    background_dim.set_opacity(0.75);
    window_overlay.add_overlay(&background_dim);
    window_overlay.add_overlay(&toast_overlay);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lookout")
        .default_width(1600)
        .default_height(900)
        .content(&window_overlay)
        .build();
    // The message list's sort menu is built long before the window exists;
    // `win.`-scoped actions resolve through the widget hierarchy when the menu
    // item is activated, so registering it here is in time.
    window.add_action(&sort_key_action);
    window.add_action(&list_filter_action);

    // --- Close-to-background: the window's close button hides the window
    // instead of exiting (Config → General → "Keep running when the window
    // is closed"), so account sync and the notification loops keep running;
    // File → Quit (`app.quit`) is the real exit. The portal status line
    // (v2 `SetStatus`) tells the shell's background-apps list what the
    // hidden app is doing; clearing it on show keeps that list honest.
    {
        let settings = settings.clone();
        window.connect_close_request({
            let worker = worker.clone();
            move |win| {
                if !settings.get_bool(crate::settings::CLOSE_TO_BACKGROUND) {
                    return glib::Propagation::Proceed;
                }
                worker.spawn(crate::background::set_background_status("Syncing your mail and calendar"));
                win.set_visible(false);
                glib::Propagation::Stop
            }
        });
        window.connect_show({
            let worker = worker.clone();
            move |_| {
                worker.spawn(crate::background::set_background_status(""));
            }
        });
    }

    // --- Pane widths: persist and reapply. Each paned's `position` change
    // (a horizontal drag) cancels the pending settle and reschedules it, so
    // 150 ms after the drag stops the pane's width as a percentage of the
    // window width is stored in GSettings.
    // When the window itself is resized, the stored percentages are applied
    // back to the panes (clamped to each pane's min/max widths, which is what
    // the drag itself abides by). ---
    let apply_stored_pane_widths = {
        let main_paned = main_paned.clone();
        let messages_reading_paned = messages_reading_paned.clone();
        let calendar_paned = calendar_paned.clone();
        let contacts_paned = contacts_paned.clone();
        let config_paned = config_view.paned.clone();
        let state = state.clone();
        move |window_width: i32| {
            if window_width <= 0 {
                return;
            }
            let settings = state.borrow().settings.clone();
            let folder_pct = settings.get_double(crate::settings::PANE_FOLDER_WIDTH_PCT);
            if folder_pct > 0.0 && main_paned.is_mapped() {
                let start_min = main_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let end_min = main_paned.end_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let min = start_min;
                let max = main_paned.width().saturating_sub(end_min).max(min).min(FOLDER_PANE_MAX_WIDTH);
                let target = (folder_pct / 100.0 * window_width as f64) as i32;
                main_paned.set_position(target.clamp(min, max));
            }
            let list_pct = settings.get_double(crate::settings::PANE_MESSAGE_LIST_WIDTH_PCT);
            if list_pct > 0.0 && messages_reading_paned.is_mapped() {
                let start_min = messages_reading_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let end_min = messages_reading_paned.end_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let min = start_min;
                let max = messages_reading_paned.width().saturating_sub(end_min).max(min);
                let target = (list_pct / 100.0 * window_width as f64) as i32;
                messages_reading_paned.set_position(target.clamp(min, max));
            }
            let calendar_pct = settings.get_double(crate::settings::PANE_CALENDAR_SIDEBAR_WIDTH_PCT);
            if calendar_pct > 0.0 && calendar_paned.is_mapped() {
                let start_min = calendar_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let end_min = calendar_paned.end_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let min = start_min;
                let max = calendar_paned.width().saturating_sub(end_min).max(min);
                let target = (calendar_pct / 100.0 * window_width as f64) as i32;
                calendar_paned.set_position(target.clamp(min, max));
            }
            let contacts_pct = settings.get_double(crate::settings::PANE_CONTACTS_SIDEBAR_WIDTH_PCT);
            if contacts_pct > 0.0 && contacts_paned.is_mapped() {
                let start_min = contacts_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let end_min = contacts_paned.end_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let min = start_min;
                let max = contacts_paned.width().saturating_sub(end_min).max(min);
                let target = (contacts_pct / 100.0 * window_width as f64) as i32;
                contacts_paned.set_position(target.clamp(min, max));
            }
            let config_pct = settings.get_double(crate::settings::PANE_CONFIG_SIDEBAR_WIDTH_PCT);
            if config_pct > 0.0 && config_paned.is_mapped() {
                let start_min = config_paned.start_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let end_min = config_paned.end_child().map(|w| w.measure(gtk::Orientation::Horizontal, -1).0).unwrap_or(0);
                let min = start_min;
                let max = config_paned.width().saturating_sub(end_min).max(min);
                let target = (config_pct / 100.0 * window_width as f64) as i32;
                config_paned.set_position(target.clamp(min, max));
            }
        }
    };
    {
        let window_for_debug = window.clone();
        let state_for_save = state.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        main_paned.connect_notify_local(Some("position"), move |paned, _| {
            // Cap the folder pane's width: the drag itself is only bounded
            // by the paned's natural minimums, so snap any overshoot here.
            // Returning lets the re-entrant notify (from `set_position`)
            // run the settle/save logic with the capped value.
            if paned.position() > FOLDER_PANE_MAX_WIDTH {
                paned.set_position(FOLDER_PANE_MAX_WIDTH);
                return;
            }
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let width = paned.position();
            let window_width = window_for_debug.width();
            let state_for_timeout = state_for_save.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce_for_timeout.set(None);
                if window_width > 0 {
                    let pct = width as f64 * 100.0 / window_width as f64;
                    state_for_timeout.borrow().settings.set_double(crate::settings::PANE_FOLDER_WIDTH_PCT, pct);
                }
            })));
        });
        let window_for_debug = window.clone();
        let state_for_save = state.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        messages_reading_paned.connect_notify_local(Some("position"), move |paned, _| {
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let width = paned.position();
            let window_width = window_for_debug.width();
            let state_for_timeout = state_for_save.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce_for_timeout.set(None);
                if window_width > 0 {
                    let pct = width as f64 * 100.0 / window_width as f64;
                    state_for_timeout.borrow().settings.set_double(crate::settings::PANE_MESSAGE_LIST_WIDTH_PCT, pct);
                }
            })));
        });
        let window_for_debug = window.clone();
        let state_for_save = state.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        calendar_paned.connect_notify_local(Some("position"), move |paned, _| {
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let width = paned.position();
            let window_width = window_for_debug.width();
            let state_for_timeout = state_for_save.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce_for_timeout.set(None);
                if window_width > 0 {
                    let pct = width as f64 * 100.0 / window_width as f64;
                    state_for_timeout.borrow().settings.set_double(crate::settings::PANE_CALENDAR_SIDEBAR_WIDTH_PCT, pct);
                }
            })));
        });
        let window_for_debug = window.clone();
        let state_for_save = state.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        contacts_paned.connect_notify_local(Some("position"), move |paned, _| {
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let width = paned.position();
            let window_width = window_for_debug.width();
            let state_for_timeout = state_for_save.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce_for_timeout.set(None);
                if window_width > 0 {
                    let pct = width as f64 * 100.0 / window_width as f64;
                    state_for_timeout.borrow().settings.set_double(crate::settings::PANE_CONTACTS_SIDEBAR_WIDTH_PCT, pct);
                }
            })));
        });
        let window_for_debug = window.clone();
        let state_for_save = state.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        config_view.paned.connect_notify_local(Some("position"), move |paned, _| {
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let width = paned.position();
            let window_width = window_for_debug.width();
            let state_for_timeout = state_for_save.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce_for_timeout.set(None);
                if window_width > 0 {
                    let pct = width as f64 * 100.0 / window_width as f64;
                    state_for_timeout.borrow().settings.set_double(crate::settings::PANE_CONFIG_SIDEBAR_WIDTH_PCT, pct);
                }
            })));
        });
    }
    // GTK4 only updates `default-width` when the window is resized while
    // resizable and not maximized/tiled/fullscreen (see `should_remember_size`
    // in gtkwindow.c), so instead we listen on the window's GdkSurface
    // `width`, which is updated on every surface resize in every state. The
    // surface only exists once the window is realized, so it's wired from the
    // window's `map` signal (guarded so re-maps don't stack handlers). The
    // width is read at timeout time - by then the new allocation has settled,
    // so the applied percentages are against the final window size. Debounced
    // so the stored percentages are applied once the window stops resizing.
    {
        let apply_stored_pane_widths = apply_stored_pane_widths.clone();
        let check_overview_fits = check_overview_fits.clone();
        let wired: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        window.connect_map(move |window| {
            let Some(surface) = window.surface() else {
                return;
            };
            if wired.get() {
                return;
            }
            wired.set(true);
            let apply_for_notify = apply_stored_pane_widths.clone();
            let check_for_notify = check_overview_fits.clone();
            let window_for_width = window.clone();
            let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
            surface.connect_width_notify(move |_| {
                if let Some(id) = debounce.take() {
                    id.remove();
                }
                let window_for_timeout = window_for_width.clone();
                let apply_for_timeout = apply_for_notify.clone();
                let check_for_timeout = check_for_notify.clone();
                let debounce_for_timeout = debounce.clone();
                debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    debounce_for_timeout.set(None);
                    apply_for_timeout(window_for_timeout.width());
                    check_for_timeout();
                })));
            });
            // The window can also come up already too narrow (a restored
            // geometry) with no subsequent resize to trigger the check, so
            // run it once the initial allocation settles too.
            let check_initial = check_overview_fits.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), check_initial);
        });
    }
    // The calendar and people panes live in `root_stack`, so they're only
    // mapped while their tab is the visible one. The window-resize handler
    // above therefore skips them whenever the window is resized on another
    // tab, so the stored percentages are also reapplied each time the stack
    // switches to a page holding a paned split. Debounced like the resize
    // handler so the panes' allocations have settled before the positions
    // are computed.
    {
        let root_stack = root_stack.clone();
        let apply_stored_pane_widths = apply_stored_pane_widths.clone();
        let window_for_timeout = window.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        root_stack.connect_notify_local(Some("visible-child-name"), move |_, _| {
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let window_for_timeout = window_for_timeout.clone();
            let apply_for_timeout = apply_stored_pane_widths.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                debounce_for_timeout.set(None);
                apply_for_timeout(window_for_timeout.width());
            })));
        });
    }

    // --- Ctrl+F is a global keyboard shortcut now: see the shortcuts block
    // after the nav-rail buttons below (it dispatches through the same
    // `EventControllerKey` as every other chord). ---

    let state = state.clone();
    // The UI-state database also holds local-only tasks - best-effort, like
    // the contacts code's handle: a failed open means local tasks live in
    // memory for this session only.
    let local_tasks_db = UiStateDb::open()
        .map(|db| Rc::new(RefCell::new(db)))
        .inspect_err(|e| tracing::warn!("local tasks won't persist: {e}"))
        .ok();
    let local_tasks = local_tasks_db.as_ref().and_then(|db| db.borrow().load_local_tasks().ok()).unwrap_or_default();
    let calendar_state = Rc::new(RefCell::new(CalendarUiState {
        accounts: HashMap::new(),
        displayed_month: current_month_start(),
        checked_calendar_ids: HashSet::new(),
        calendar_colors: calendar_colors::load(),
        webcal_cmd_tx: None,
        webcal_subscriptions: Vec::new(),
        webcal_handles: HashMap::new(),
        birthdays: None,
        google_tasks: HashMap::new(),
        google_account_emails: Vec::new(),
        local_tasks,
        local_tasks_db,
        dashboard_refresh: None,
        task_button_refresh: None,
        pending_calendar_moves: HashMap::new(),
        mail_overview_activate: None,
        mail_overview_refresh: None,
    }));

    // --- Lookout dashboard refresh hook: the window registers one closure
    // that repaints the whole dashboard, and hands it to the mail sessions
    // (which refresh it on folder/message syncs) and into `calendar_state`
    // (where `refresh_tasks_view` and the calendar event loops reach it).
    // The tab-open and toolbar-Refresh handlers call `refresh_lookout_view`
    // directly instead (see below) - they represent the dashboard becoming
    // visible or an explicit user ask, so they must run immediately, not
    // wait out this hook's debounce or bail out on its visibility check.
    //
    // Debounced (500ms trailing edge, the pane-resize save's idiom at
    // `main_paned.connect_notify_local` above) and skipped entirely while
    // the dashboard isn't the visible tab - a sync burst across several
    // accounts would otherwise repaint the (per-account, whole-`messages`-
    // table) histogram/top-contacts scan once per event, on screen or not.
    let dashboard_refresh: Rc<dyn Fn()> = {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let lookout_view = lookout_view.clone();
        let lookout_view_button = lookout_view_button.clone();
        let debounce: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));
        Rc::new(move || {
            if !lookout_view_button.is_active() {
                return;
            }
            if let Some(id) = debounce.take() {
                id.remove();
            }
            let state = state.clone();
            let calendar_state = calendar_state.clone();
            let lookout_view = lookout_view.clone();
            let debounce_for_timeout = debounce.clone();
            debounce.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                debounce_for_timeout.set(None);
                refresh_lookout_view(&state, &calendar_state, &lookout_view);
            })));
        })
    };
    calendar_state.borrow_mut().dashboard_refresh = Some(dashboard_refresh.clone());

    // --- "Add as Task" flag button's own repaint hook, same registration
    // pattern as `dashboard_refresh` just above - `refresh_tasks_view` calls
    // it so the button's icon tracks whether the *currently selected*
    // message already has an associated task, even when that task was just
    // created, synced in from the server, or removed.
    let task_button_refresh: Rc<dyn Fn()> = {
        let task_button = task_button.clone();
        let message_list = message_list.clone();
        let calendar_state = calendar_state.clone();
        Rc::new(move || refresh_task_button(&task_button, &message_list, &calendar_state))
    };
    calendar_state.borrow_mut().task_button_refresh = Some(task_button_refresh.clone());
    task_button_refresh();

    // --- Calendar event reminders. The engine accumulates every occurrence
    // the sessions report (via `connect_calendar_account`'s ingest) and the
    // loop fires `Gio.Notification`s for due alerts, with Open / Snooze /
    // Dismiss actions; `open_event` lands on the shared event editor. The
    // persistence handle is a second best-effort open of the UI-state
    // database (the one in `UiState` is owned by the contacts code) - a
    // failed open just means fire-once state doesn't survive restarts.
    let reminders_engine = Rc::new(RefCell::new(crate::reminders::ReminderEngine::new(
        crate::ui_state_db::UiStateDb::open()
            .map(|db| Rc::new(RefCell::new(db)))
            .inspect_err(|e| tracing::warn!("reminder state won't persist: {e}"))
            .ok(),
    )));
    crate::reminders::spawn_reminder_loop(app, reminders_engine.clone(), settings.clone(), {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let calendar_view_button = calendar_view_button.clone();
        Rc::new(move |occ: EventOccurrence| {
            window.present();
            calendar_view_button.set_active(true);
            open_event_editor_for(&window, &state, &worker, &calendar_state, &occ);
        })
    });
    // --- Mail notification actions. `app.raise-window` and `app.open-mailbox`
    // are the click targets `mail_notifications::show_new_mail_notification`/
    // `show_send_failed_notification` set as their default action; both just
    // reuse existing navigation rather than duplicating it. Selecting the row
    // programmatically fires the same `connect_selected_item_notify` handler
    // (below) a real sidebar click does, so it gets `select_mailbox` /
    // `exit_search` / `refresh_list_header` for free.
    crate::mail_notifications::spawn_actions(
        app,
        {
            let window = window.clone();
            let folder_selection = folder_selection.clone();
            Rc::new(move |mailbox_id: MailboxId| {
                window.present();
                if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                    if let Some(index) = find_mailbox_index(&model, &mailbox_id) {
                        folder_selection.set_selected(index);
                    }
                }
            })
        },
        {
            let window = window.clone();
            Rc::new(move || window.present())
        },
    );
    // --- iMIP banner actions. The banner's button acts on the invitation
    // stashed in `UiState::imip` by `render_body`; the calendar half of the
    // response (saving an accepted event, removing a cancelled one) needs
    // `calendar_state`, which is why this handler is registered here rather
    // than alongside the banner widget's construction. REQUEST opens a
    // three-way response dialog, CANCEL a remove-from-calendar confirmation,
    // and REPLY is informational (the button just dismisses). Acting on the
    // banner hides it for that message, like the unsubscribe banner.
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        imip_banner.connect_button_clicked(move |banner| {
            let (mailbox, uid, invitation, from_email, display_name, cmd_tx) = {
                let st = state.borrow();
                let (mailbox, uid) = match &st.rendered_message {
                    Some(rendered) => rendered.clone(),
                    None => return,
                };
                let Some(invitation) = st.imip.clone() else { return };
                let Some(account_id) = mailbox_account_id(&mailbox) else { return };
                let Some(handle) = st.accounts.get(&account_id) else { return };
                (mailbox, uid, invitation, handle.email.clone(), handle.display_name.clone(), handle.cmd_tx.clone())
            };
            let dismiss = |state: &Rc<RefCell<UiState>>, banner: &adw::Banner, mailbox: &MailboxId, uid: Uid| {
                state.borrow_mut().imip_dismissed = Some((mailbox.clone(), uid));
                banner.set_revealed(false);
            };
            match invitation.method {
                lookout_core::ImipMethod::Request => {
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Invitation: {}", invitation.summary.as_deref().unwrap_or("an event")))
                        .body(if let Some(organizer) = &invitation.organizer {
                            format!("Responding will send your answer to {}.", organizer.display_label())
                        } else {
                            "Responding will send your answer to the organizer.".to_string()
                        })
                        .default_response("accept")
                        .close_response("cancel")
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("decline", "Decline");
                    dialog.add_response("tentative", "Maybe");
                    dialog.add_response("accept", "Accept");
                    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
                    dialog.set_response_appearance("decline", adw::ResponseAppearance::Destructive);
                    let state_for_dialog = state.clone();
                    let calendar_state_for_dialog = calendar_state.clone();
                    let toast_overlay_for_dialog = toast_overlay.clone();
                    let cmd_tx_for_dialog = cmd_tx.clone();
                    let from_email_for_dialog = from_email.clone();
                    let display_name_for_dialog = display_name.clone();
                    let invitation_for_dialog = invitation.clone();
                    let mailbox_for_dialog = mailbox.clone();
                    let banner_for_dialog = banner.clone();
                    dialog.connect_response(None, move |_dialog, response| {
                        let status = match response {
                            "accept" => Some(lookout_core::AttendeeStatus::Accepted),
                            "tentative" => Some(lookout_core::AttendeeStatus::Tentative),
                            "decline" => Some(lookout_core::AttendeeStatus::Declined),
                            _ => None,
                        };
                        let Some(status) = status else { return };
                        respond_to_imip_invitation(
                            &calendar_state_for_dialog,
                            &toast_overlay_for_dialog,
                            &invitation_for_dialog,
                            &from_email_for_dialog,
                            Some(display_name_for_dialog.as_str()),
                            &cmd_tx_for_dialog,
                            status,
                        );
                        state_for_dialog.borrow_mut().imip_dismissed = Some((mailbox_for_dialog.clone(), uid));
                        banner_for_dialog.set_revealed(false);
                    });
                    dialog.present(Some(banner));
                }
                lookout_core::ImipMethod::Cancel => {
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Cancelled: {}", invitation.summary.as_deref().unwrap_or("an event")))
                        .body("The organizer cancelled this event. Remove it from your calendar?")
                        .default_response("keep")
                        .close_response("keep")
                        .build();
                    dialog.add_response("keep", "Keep");
                    dialog.add_response("remove", "Remove from calendar");
                    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                    let state_for_dialog = state.clone();
                    let calendar_state_for_dialog = calendar_state.clone();
                    let toast_overlay_for_dialog = toast_overlay.clone();
                    let invitation_for_dialog = invitation.clone();
                    let mailbox_for_dialog = mailbox.clone();
                    let banner_for_dialog = banner.clone();
                    dialog.connect_response(None, move |_dialog, response| {
                        dismiss(&state_for_dialog, &banner_for_dialog, &mailbox_for_dialog, uid);
                        if response == "remove" {
                            remove_cancelled_imip_event(&calendar_state_for_dialog, &toast_overlay_for_dialog, &invitation_for_dialog);
                        }
                    });
                    dialog.present(Some(banner));
                }
                lookout_core::ImipMethod::Reply => {
                    dismiss(&state, banner, &mailbox, uid);
                }
            }
        });
    }
    // Which single day the Mail-screen overview pane's day list is
    // currently showing - separate from `calendar_state.displayed_month`
    // (that's the main Calendar view's own concern).
    let mail_overview_day: Rc<Cell<chrono::NaiveDate>> = Rc::new(Cell::new(chrono::Utc::now().date_naive()));
    refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);

    // --- Mail-screen overview pane's task rows + repaint hook, registered
    // here (same pattern as `dashboard_refresh`/`task_button_refresh` just
    // above) so `refresh_mail_overview_day_list` can build task rows - a row
    // click opens the shared task editor, reusing the Tasks view's session
    // paths (the overview's rows carry no completion checkbox). The pane's
    // repaint hook fires from `refresh_tasks_view`, so every task change
    // (synced, saved, toggled, deleted) updates the pane.
    {
        let calendar_state_for_activate = calendar_state.clone();
        let tasks_view_for_activate = tasks_view.clone();
        let mail_overview_refresh: Rc<dyn Fn()> = {
            let calendar_state = calendar_state.clone();
            let mail_overview_day = mail_overview_day.clone();
            let mail_overview_day_list = mail_overview_day_list.clone();
            Rc::new(move || refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list))
        };
        let mut st = calendar_state.borrow_mut();
        st.mail_overview_refresh = Some(mail_overview_refresh);
        let window_for_activate = window.clone();
        st.mail_overview_activate = Some(Rc::new(move |task| {
            open_task_editor_for(&window_for_activate, &calendar_state_for_activate, &tasks_view_for_activate, &task)
        }));
    }

    let contacts_categories: Rc<RefCell<Vec<ContactsCategoryChoice>>> = Rc::new(RefCell::new(Vec::new()));
    let contacts_entries: Rc<RefCell<Vec<ContactsListEntry>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let contacts_categories = contacts_categories.clone();
        let contacts_entries = contacts_entries.clone();
        let contacts_list = contacts_list.clone();
        let state = state.clone();
        contacts_category_list.connect_row_selected(move |_list, row| {
            let Some(row) = row else { return };
            // Header rows ("gavindi@outlook.com", "Categories") sit in the
            // list alongside selectable rows, so a row's position in the
            // widget no longer matches its index in `contacts_categories` -
            // that index is stashed on the row itself in
            // `refresh_contacts_category_ui`.
            let Some(idx) = (unsafe { row.data::<usize>("contacts-choice-index") }) else { return };
            let idx = unsafe { *idx.as_ref() };
            rebuild_contacts_list_ui(&contacts_list, &contacts_categories, &contacts_entries, idx as i32, &state);
        });
    }
    let refresh_contacts_ui: Rc<dyn Fn(Option<i32>)> = Rc::new({
        let state = state.clone();
        let contacts_category_list = contacts_category_list.clone();
        let contacts_list = contacts_list.clone();
        let contacts_categories = contacts_categories.clone();
        let contacts_entries = contacts_entries.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_sidebar.calendar_list_box.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let reminders_engine = reminders_engine.clone();
        move |selected_index: Option<i32>| {
            refresh_contacts_category_ui(&state, &contacts_category_list, &contacts_list, &contacts_categories, &contacts_entries, selected_index);
            // Contacts are the birthdays calendar's only data source, so
            // every snapshot (startup cache paint, poll tick, post-write
            // resync) refreshes the synthesized calendar and everything that
            // renders it - the same funnel the webcal session uses.
            if refresh_birthdays_from_contacts(&state, &calendar_state, &reminders_engine) {
                refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                refresh_dashboard_hook(&calendar_state);
                let displayed = calendar_state.borrow().displayed_month;
                if calendar_view::mini_month(&mini_calendar) == displayed {
                    let event_days = calendar_event_days(&calendar_state, displayed);
                    calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                }
            }
        }
    });
    refresh_contacts_ui(None);
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let contacts_entries = contacts_entries.clone();
        contacts_list.connect_row_activated(move |_list, row| {
            let Some(entry) = contacts_entries.borrow().get(row.index() as usize).cloned() else {
                return;
            };
            if entry.href.is_empty() {
                // Deleted-bucket cards are display-only (their server
                // metadata is gone with the card) - keep the read-only
                // details dialog for them.
                show_contact_details_dialog(&window, &entry);
            } else {
                show_contact_editor_for(&window, &state, &toast_overlay, &entry);
            }
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        new_contact_button.connect_clicked(move |_| {
            show_new_contact_editor(&window, &state, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        manage_groups_button.connect_clicked(move |_| {
            show_manage_groups_dialog(&window, &state, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        import_contacts_button.connect_clicked(move |_| {
            show_contacts_import_dialog(&window, &state, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let contacts_entries = contacts_entries.clone();
        let toast_overlay = toast_overlay.clone();
        export_contacts_button.connect_clicked(move |_| {
            export_current_contacts(&window, &contacts_entries, &toast_overlay);
        });
    }

    // --- Config view: the third nav-rail view, a read-only overview of the
    // connected Mail/Calendar accounts (endpoints included, so it shows how
    // each account is configured) plus the Phase 5 placeholder sections, and
    // an "Add account" entry that opens GOA settings - same invocation as the
    // empty-state page's button. The account groups are repopulated by
    // `refresh_config` on every activation and again whenever either
    // discovery lands (`spawn_*_discovery` below). `config_view` itself is
    // built earlier, alongside `contacts_paned` - see the comment there.
    let config_card = card_section(&config_view.root);
    config_card.add_css_class("folder-pane");
    root_stack.add_named(&config_card, Some("config"));

    // Config → Appearance → "Animate transitions": flips the reading pane's
    // crossfade on/off live. Session-only state until Phase 5's GSettings
    // lands; off sets the transition type to `None`, which also makes
    // `render_body` skip its fade-specific dance (see below).
    {
        let reading_stack = reading_stack.clone();
        let state = state.clone();
        config_view.animations_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::ANIMATE_TRANSITIONS, row.is_active());
            let transition = if row.is_active() {
                gtk::StackTransitionType::Crossfade
            } else {
                gtk::StackTransitionType::None
            };
            reading_stack.set_transition_type(transition);
        });
    }

    // Config → Appearance → "Theme" / "Custom accent color": live re-theme.
    // Every change writes through to GSettings and re-applies the theme stack
    // (base palette + selected theme + accent), so the effect is immediate.
    // The accent picker only feeds the stack while its switch is on; the
    // switch's own handler re-applies with the system accent when turned off.
    {
        let theme_manager = theme_manager.clone();
        let state = state.clone();
        config_view.theme_row.connect_selected_notify(move |row| {
            let theme_id = crate::theme::theme_at(row.selected());
            state.borrow().settings.set_string(crate::theme::THEME_ID_KEY, theme_id);
            let accent = state.borrow().settings.get_string(crate::theme::ACCENT_COLOR_KEY);
            theme_manager.apply(theme_id, Some(&accent));
        });
    }
    {
        let theme_manager = theme_manager.clone();
        let state = state.clone();
        let accent_color_row = config_view.accent_color_row.clone();
        let accent_color_button = config_view.accent_color_button.clone();
        config_view.accent_switch_row.connect_active_notify(move |row| {
            accent_color_row.set_sensitive(row.is_active());
            let stored = if row.is_active() {
                crate::theme::rgba_to_stored(&accent_color_button.rgba())
            } else {
                String::new()
            };
            state.borrow().settings.set_string(crate::theme::ACCENT_COLOR_KEY, &stored);
            let theme_id = state.borrow().settings.get_string(crate::theme::THEME_ID_KEY);
            theme_manager.apply(&theme_id, Some(&stored));
        });
    }
    {
        let theme_manager = theme_manager.clone();
        let state = state.clone();
        let accent_switch_row = config_view.accent_switch_row.clone();
        config_view.accent_color_button.connect_rgba_notify(move |button| {
            if !accent_switch_row.is_active() {
                return;
            }
            let stored = crate::theme::rgba_to_stored(&button.rgba());
            state.borrow().settings.set_string(crate::theme::ACCENT_COLOR_KEY, &stored);
            let theme_id = state.borrow().settings.get_string(crate::theme::THEME_ID_KEY);
            theme_manager.apply(&theme_id, Some(&stored));
        });
    }

    // Config → Appearance → "Dark message theme": the default state for
    // *newly-opened* messages only - the reading pane's per-message
    // "Switch message theme" toggle still overrides the current message
    // either way, so the change applies on the next navigation rather than
    // re-rendering (and possibly yanking) the message on screen.
    {
        let state = state.clone();
        config_view.message_theme_dark_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_MESSAGE_THEME_DARK, row.is_active());
        });
    }

    // Config → Mail → "Load images from the web": flips the WebView's
    // remote-image veto live, then re-renders whatever's on the reading pane
    // so the change applies to the open message, not just the next selection.
    // Skipped while a composer is up - `render_body` would route the pane
    // back to the message page and yank the user out of their draft.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        let message_list = message_list.clone();
        config_view.remote_images_row.connect_active_notify(move |row| {
            state.borrow_mut().load_remote_images = row.is_active();
            state.borrow().settings.set_bool(crate::settings::MAIL_LOAD_REMOTE_IMAGES, row.is_active());
            rerender_current_message(&state, &reading_stack, &message_header, &message_list);
        });
    }

    // Config → Mail → "Trusted senders…": opens the manage dialog for the
    // external-content trust flow (add/remove sender entries, change trust
    // levels). Every change writes through to the UI-state database and the
    // in-memory `trusted_senders` mirror, so the reading pane's load policy
    // picks it up on the next decision.
    {
        let state = state.clone();
        let row = config_view.trusted_senders_row.clone();
        row.clone().connect_activated(move |_| {
            crate::trusted_senders::show_manage_dialog(row.upcast_ref::<gtk::Widget>(), state.clone());
        });
    }

    // Config → Mail → "Rich text": sets the default body mode for future
    // compose sessions. Read at composer-open time, so an already-open
    // composer is untouched.
    {
        let state = state.clone();
        config_view.rich_text_row.connect_active_notify(move |row| {
            state.borrow_mut().rich_text_default = row.is_active();
            state.borrow().settings.set_bool(crate::settings::MAIL_RICH_TEXT_DEFAULT, row.is_active());
        });
    }

    // Config → Mail → "Send read receipts automatically": the reading pane's
    // read-receipt policy. Read per message at display time, so a flip
    // affects the next message opened - no re-render needed.
    {
        let state = state.clone();
        config_view.read_receipts_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_SEND_READ_RECEIPTS, row.is_active());
        });
    }

    // Config → Mail → "Mail notifications": gates the new-mail/send-failure
    // desktop notifications. Read straight off the setting at the point each
    // event fires (see `connect_account`), so nothing needs re-arming here.
    {
        let state = state.clone();
        config_view.mail_notifications_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED, row.is_active());
        });
    }

    // Config → Calendar → "Event alerts": gates the reminder loop. The loop
    // (see `reminders::spawn_reminder_loop`) reads the key on every tick, so
    // nothing needs re-arming here - disabling mid-session simply stops new
    // notifications (and stops marking alerts as shown, so re-enabling
    // re-fires anything still due).
    {
        let state = state.clone();
        config_view.calendar_alerts_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::CALENDAR_ALERTS_ENABLED, row.is_active());
        });
    }

    // Config → General → "Start Lookout at login": registers login
    // autostart with the session's Background portal when available (the
    // shell decides, first time via a dialog, and manages the registration
    // from then on - see the row's subtitle), falling back to a managed
    // XDG autostart file when there's no portal. Disabling removes the
    // XDG entry; a portal registration is revoked from the desktop's app
    // settings, since the portal API has no unregister call.
    {
        let state = state.clone();
        let worker = worker.clone();
        config_view.start_at_login_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            state.borrow().settings.set_bool(crate::settings::START_AT_LOGIN, enabled);
            if enabled {
                worker.spawn(async {
                    let _ = crate::background::enable_login_autostart().await;
                });
            } else {
                crate::background::disable_login_autostart();
            }
        });
    }

    // Config → General → "Keep running when the window is closed": read by
    // the main window's close-request handler at close time (see the
    // `connect_close_request` above), so nothing needs re-arming here.
    {
        let state = state.clone();
        config_view.close_to_background_row.connect_active_notify(move |row| {
            state.borrow().settings.set_bool(crate::settings::CLOSE_TO_BACKGROUND, row.is_active());
        });
    }
    // Phase 5: apply the persisted Config → Appearance/Mail switch states now
    // that their handlers are wired. Each `set_active` fires the notify
    // handler above, which re-derives UiState and any widget effect from the
    // new value, so startup can't drift from what was saved.
    {
        let persisted = state.borrow().settings.clone();
        config_view.animations_row.set_active(persisted.get_bool(crate::settings::ANIMATE_TRANSITIONS));
        config_view.message_theme_dark_row.set_active(persisted.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK));
        config_view.remote_images_row.set_active(persisted.get_bool(crate::settings::MAIL_LOAD_REMOTE_IMAGES));
        config_view.rich_text_row.set_active(persisted.get_bool(crate::settings::MAIL_RICH_TEXT_DEFAULT));
        config_view.read_receipts_row.set_active(persisted.get_bool(crate::settings::MAIL_SEND_READ_RECEIPTS));
        config_view
            .mail_notifications_row
            .set_active(persisted.get_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED));
        config_view.calendar_alerts_row.set_active(persisted.get_bool(crate::settings::CALENDAR_ALERTS_ENABLED));
        // "Start Lookout at login": the setting is the source of truth for
        // the portal path; the managed XDG file is the fallback, so an
        // entry left over (or written while the portal was absent) counts
        // as enabled too. Seeding fires the notify handler above, which for
        // an enabled start re-asserts the registration - a silent no-op
        // when the portal already knows.
        if crate::background::autostart_file_exists() && !persisted.get_bool(crate::settings::START_AT_LOGIN) {
            persisted.set_bool(crate::settings::START_AT_LOGIN, true);
        }
        config_view.start_at_login_row.set_active(persisted.get_bool(crate::settings::START_AT_LOGIN));
        config_view.close_to_background_row.set_active(persisted.get_bool(crate::settings::CLOSE_TO_BACKGROUND));
        // Theme rows: seeding fires the notify handlers above, which apply
        // the persisted theme/accent through the ThemeManager, so startup
        // can't drift from what was saved.
        let theme_id = persisted.get_string(crate::theme::THEME_ID_KEY);
        config_view.theme_row.set_selected(crate::theme::theme_index(&theme_id));
        let accent = persisted.get_string(crate::theme::ACCENT_COLOR_KEY);
        if let Some(rgba) = crate::theme::accent_rgba(&accent) {
            config_view.accent_color_button.set_rgba(&rgba);
        }
        config_view.accent_switch_row.set_active(!accent.is_empty());
    }

    {
        let add_account_row = config_view.add_account_row.clone();
        let worker = worker.clone();
        add_account_row.connect_activated(move |_| {
            worker.spawn(crate::online_accounts::open_online_accounts());
        });
    }

    // Config → Appearance → "Window background": reflect a stored custom
    // background (if one applied at startup) in the row subtitle and arm the
    // restore row, seed the (always-enabled) dimming slider with the stored
    // brightness regardless of which background is in use, then wire the
    // picker to a file chooser, the dimming slider into the background's
    // brightness, and "Restore default background" back to the bundled
    // artwork (which also resets dimming to its own default).
    let apply_background_brightness = {
        let background_dim = background_dim.clone();
        move |brightness: f64| background_dim.set_opacity(1.0 - brightness.clamp(0.0, 1.0))
    };
    {
        let brightness = settings.get_double(crate::settings::BACKGROUND_BRIGHTNESS);
        config_view.background_brightness_scale.set_value(brightness);
        apply_background_brightness(brightness);
    }
    if let Some(name) = &custom_background_name {
        config_view.background_image_row.set_subtitle(name);
        config_view.restore_background_row.set_sensitive(true);
    }
    {
        let background_image_row = config_view.background_image_row.clone();
        let restore_background_row = config_view.restore_background_row.clone();
        let background_brightness_scale = config_view.background_brightness_scale.clone();
        let background = background.clone();
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let settings = settings.clone();
        background_image_row.connect_activated(move |row| {
            let row = row.clone();
            let window = window.clone();
            let background = background.clone();
            let toast_overlay = toast_overlay.clone();
            let restore_background_row = restore_background_row.clone();
            let background_brightness_scale = background_brightness_scale.clone();
            let settings = settings.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_pixbuf_formats();
                filter.set_name(Some("Images"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);

                let dialog = gtk::FileDialog::builder().title("Choose a background image").filters(&filters).build();
                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                match gtk::gdk::Texture::from_filename(&path) {
                    Ok(texture) => {
                        background.set_paintable(Some(&texture));
                        crate::background_image::save(&settings, &path);
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
                        row.set_subtitle(&name);
                        restore_background_row.set_sensitive(true);
                        // Every freshly-picked image starts at 50% brightness
                        // (the `value-changed` handler persists it and applies
                        // it to the overlay).
                        background_brightness_scale.set_value(0.5);
                    }
                    Err(e) => {
                        let title = glib::markup_escape_text(&format!("Couldn't load background image: {e}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                }
            });
        });
    }
    {
        let background_brightness_scale = config_view.background_brightness_scale.clone();
        let settings = settings.clone();
        let apply_background_brightness = apply_background_brightness.clone();
        background_brightness_scale.connect_value_changed(move |scale| {
            let brightness = scale.value();
            settings.set_double(crate::settings::BACKGROUND_BRIGHTNESS, brightness);
            apply_background_brightness(brightness);
        });
    }
    {
        let restore_background_row = config_view.restore_background_row.clone();
        let background_image_row = config_view.background_image_row.clone();
        let background_brightness_scale = config_view.background_brightness_scale.clone();
        let background = background.clone();
        let settings = settings.clone();
        let apply_background_brightness = apply_background_brightness.clone();
        restore_background_row.connect_activated(move |row| {
            crate::background_image::clear(&settings);
            background.set_paintable(Some(&default_bg_texture));
            background_image_row.set_subtitle("Default Lookout artwork");
            // The bundled artwork defaults to 75% brightness: reset the
            // stored dim value so the slider reflects it too.
            settings.set_double(crate::settings::BACKGROUND_BRIGHTNESS, 0.75);
            background_brightness_scale.set_value(0.75);
            apply_background_brightness(0.75);
            row.set_sensitive(false);
        });
    }

    // --- Config's own command-toolbar row, swapped in via `view_toolbar_stack`
    // like Mail's and Calendar's when the Config nav-rail button is active.
    let config_add_account_button = gtk::Button::from_icon_name("contact-new-symbolic");
    config_add_account_button.set_tooltip_text(Some("Add account"));
    {
        let worker = worker.clone();
        config_add_account_button.connect_clicked(move |_| {
            worker.spawn(crate::online_accounts::open_online_accounts());
        });
    }
    let config_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    config_command_toolbar.append(&config_add_account_button);
    view_toolbar_stack.add_named(&config_command_toolbar, Some("config"));

    let config_view_button = gtk::ToggleButton::builder()
        .icon_name("preferences-system-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Config")
        .build();
    config_view_button.set_group(Some(&contacts_view_button));
    // Anchored to the bottom of the rail: Mail/Calendar stay at the top, a
    // `vexpand(true)` spacer fills the middle, Config sits below it (inside
    // the scrolled window, so a very short window scrolls rather than
    // clipping it).
    let nav_rail_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav_rail_spacer.set_vexpand(true);
    nav_rail_content.append(&nav_rail_spacer);
    nav_rail_content.append(&config_view_button);

    // --- Global keyboard shortcuts. One window-level `EventControllerKey`
    // matches every key press against `shortcuts.rs`'s physical-keycode
    // table - the logical GSettings accelerators are resolved against the
    // keymap at startup, so a chord fires from anywhere in the window
    // (message list, reading pane, composer, search entry) and is
    // independent of the active keyboard layout. Actions dispatch through
    // the toolbar/rail buttons themselves (`emit_clicked` / `set_active`),
    // so each verb keeps exactly one implementation - its button handler,
    // including the sensitivity gating that makes a shortcut with no
    // selection a silent no-op (same convention as clicking the button).
    // The Config screen's Keyboard group captures replacements through the
    // same controller: while `capturing` is set the dispatcher is bypassed
    // and presses feed `set_chord` instead. ---
    let shortcut_settings = state.borrow().settings.clone();
    let shortcuts = Rc::new(RefCell::new(crate::shortcuts::ShortcutState::load(&shortcut_settings)));
    let display = gtk::gdk::Display::default();
    if let Some(display) = &display {
        shortcuts.borrow_mut().rebuild(display);
    }
    let mut dispatch: HashMap<&'static str, Rc<dyn Fn()>> = HashMap::new();
    for (action, button) in [
        (crate::shortcuts::ACTION_COMPOSE, &compose_button),
        (crate::shortcuts::ACTION_REPLY, &reply_button),
        (crate::shortcuts::ACTION_REPLY_ALL, &reply_all_button),
        (crate::shortcuts::ACTION_FORWARD, &forward_button),
        (crate::shortcuts::ACTION_DELETE, &delete_button),
        (crate::shortcuts::ACTION_ARCHIVE, &archive_button),
        (crate::shortcuts::ACTION_REPORT_JUNK, &report_button),
        (crate::shortcuts::ACTION_SNOOZE, &snooze_button),
        (crate::shortcuts::ACTION_FLAG, &star_button),
        (crate::shortcuts::ACTION_MARK_READ, &mark_read_button),
    ] {
        let button = button.clone();
        dispatch.insert(action, Rc::new(move || button.emit_clicked()));
    }
    // Print: the reading-pane print path (the calendar toolbar's Print
    // button prints the month instead), gated the same way the More menu's
    // Print item is: only while a message is actually rendered.
    dispatch.insert(
        crate::shortcuts::ACTION_PRINT,
        Rc::new({
            let window = window.clone();
            let reading_stack = reading_stack.clone();
            move || {
                if reading_stack.visible_child_name().as_deref() == Some("message") {
                    let parent: &gtk::Window = window.upcast_ref();
                    print_visible_message(&reading_stack, parent);
                }
            }
        }),
    );
    // Find: Ctrl+F now lives in the same table as everything else (it
    // replaced the old `ShortcutController` block above). Selecting the
    // entry's existing text means typing replaces the old query.
    dispatch.insert(
        crate::shortcuts::ACTION_SEARCH,
        Rc::new({
            let search_entry = search_entry.clone();
            move || {
                search_entry.select_region(0, -1);
                search_entry.grab_focus();
            }
        }),
    );
    // Close reading pane: back to the blank pane, mirroring the state reset
    // the message-selection handler performs for "nothing selected". A
    // composer in the pane is left alone - drafts are never yanked away.
    dispatch.insert(
        crate::shortcuts::ACTION_CLOSE_PANE,
        Rc::new({
            let state = state.clone();
            let reading_stack = reading_stack.clone();
            let web_view = web_view.clone();
            let user_content_manager = user_content_manager.clone();
            let theme_override_sheet = theme_override_sheet.clone();
            move || {
                if reading_stack.visible_child_name().as_deref() == Some("compose") {
                    return;
                }
                // The message-theme override resets to the configured default
                // and its physical sheet/canvas are re-armed to match, the
                // same as every other navigation path.
                let default_dark = {
                    let mut st = state.borrow_mut();
                    st.pending_body_request = None;
                    st.pending_html_reveal = false;
                    st.reveal_generation += 1;
                    st.pending_header = None;
                    st.pending_attachment = None;
                    st.pending_raw_message = None;
                    st.unsubscribe_info = None;
                    st.unsubscribe_dismissed = None;
                    st.imip = None;
                    st.imip_dismissed = None;
                    st.read_receipt_request = None;
                    st.read_receipt_dismissed = None;
                    st.read_receipt_context = None;
                    st.rendered_trust_sender = None;
                    st.load_once_images = false;
                    st.message_theme_override = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                    st.trust_banner_dismissed = None;
                    st.rendered_message = None;
                    st.message_theme_override
                };
                drop_pending_cid(&state);
                set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
                reading_stack.set_visible_child_name("empty");
            }
        }),
    );
    // Module switching via the rail toggles: `set_active` fires the same
    // `toggled` handler a click would, and is a no-op when already there.
    for (action, button) in [
        (crate::shortcuts::ACTION_MAIL, &mail_view_button),
        (crate::shortcuts::ACTION_CALENDAR, &calendar_view_button),
        (crate::shortcuts::ACTION_CONTACTS, &contacts_view_button),
        (crate::shortcuts::ACTION_TASKS, &tasks_view_button),
        (crate::shortcuts::ACTION_LOOKOUT, &lookout_view_button),
        (crate::shortcuts::ACTION_CONFIG, &config_view_button),
    ] {
        let button = button.clone();
        dispatch.insert(action, Rc::new(move || button.set_active(true)));
    }

    // The Config keyboard rows refresh through this closure: it repaints
    // every row's accelerator label from the live table.
    let refresh_shortcut_rows: Rc<dyn Fn()> = Rc::new({
        let shortcuts = shortcuts.clone();
        let config_view = config_view.clone();
        move || {
            let table = shortcuts.borrow();
            for row in config_view.keyboard_rows.borrow().iter() {
                row.label.set_label(&table.accel_for(row.action));
            }
        }
    });

    // The action currently being recorded by the Config rows, or None.
    let capture_action: Rc<RefCell<Option<&'static str>>> = Rc::new(RefCell::new(None));

    // Row wiring: activating a row starts (or re-targets) the recording;
    // activating the row that is recording again cancels it.
    {
        let rows: Vec<_> = config_view
            .keyboard_rows
            .borrow()
            .iter()
            .map(|row| (row.row.clone(), row.label.clone(), row.action))
            .collect();
        for (row, label, action) in rows {
            let shortcuts = shortcuts.clone();
            let capture_action = capture_action.clone();
            let refresh_shortcut_rows = refresh_shortcut_rows.clone();
            row.connect_activated(move |_| {
                let capturing = shortcuts.borrow().capturing;
                if capturing && *capture_action.borrow() == Some(action) {
                    shortcuts.borrow_mut().capturing = false;
                    *capture_action.borrow_mut() = None;
                    refresh_shortcut_rows();
                    return;
                }
                *capture_action.borrow_mut() = Some(action);
                shortcuts.borrow_mut().capturing = true;
                label.set_label("Press keys…");
            });
        }
    }

    // "Restore default shortcuts": back to the shipped bindings.
    {
        let shortcuts = shortcuts.clone();
        let shortcut_settings = shortcut_settings.clone();
        let display = display.clone();
        let refresh_shortcut_rows = refresh_shortcut_rows.clone();
        config_view.reset_shortcuts_row.connect_activated(move |_| {
            let Some(display) = &display else { return };
            shortcuts.borrow_mut().reset_all(&shortcut_settings, display);
            refresh_shortcut_rows();
        });
    }

    // The single controller: dispatch matching chords, or feed the capture
    // flow while a Config row is recording. Stop on anything handled, so a
    // matched chord never also reaches the focused widget (an entry
    // retyping "n" because Ctrl+N got through would be wrong).
    {
        let shortcuts = shortcuts.clone();
        let shortcut_settings = shortcut_settings.clone();
        let capture_action = capture_action.clone();
        let refresh_shortcut_rows = refresh_shortcut_rows.clone();
        let state = state.clone();
        let dispatch = Rc::new(dispatch);
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(move |_controller, _keyval, keycode, state_modifiers| {
            if shortcuts.borrow().capturing {
                if let Some(display) = &display {
                    if let Some((modifiers, keyval)) = crate::shortcuts::chord_from_key(display, keycode, state_modifiers) {
                        // Escape (no modifiers) cancels the recording.
                        if keyval == gtk::gdk::Key::Escape && (modifiers & crate::shortcuts::MODIFIER_MASK).is_empty() {
                            shortcuts.borrow_mut().capturing = false;
                            *capture_action.borrow_mut() = None;
                            refresh_shortcut_rows();
                            return glib::Propagation::Stop;
                        }
                    }
                }
                let Some(action) = *capture_action.borrow() else {
                    return glib::Propagation::Stop;
                };
                let Some(display) = &display else {
                    return glib::Propagation::Stop;
                };
                match shortcuts.borrow_mut().set_chord(&shortcut_settings, display, action, keycode, state_modifiers) {
                    Ok(_) => {
                        shortcuts.borrow_mut().capturing = false;
                        *capture_action.borrow_mut() = None;
                        refresh_shortcut_rows();
                    }
                    Err(err) => {
                        // Stay in recording mode; the toast says what to fix.
                        let st = state.borrow();
                        if let Some(overlay) = &st.toast_overlay {
                            overlay.add_toast(adw::Toast::new(&err.message()));
                        }
                    }
                }
                return glib::Propagation::Stop;
            }
            let Some(action) = shortcuts.borrow().action_for(keycode, state_modifiers) else {
                return glib::Propagation::Proceed;
            };
            if let Some(run) = dispatch.get(action) {
                run();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(controller);
    }

    // Seed the Config rows' labels once; captures refresh them live.
    refresh_shortcut_rows();

    // The config view's manage-identities dialog refreshes through this
    // slot: it's filled with `refresh_config` right after that closure is
    // built below (a dialog's `on_changed` can't capture the closure that
    // built its anchor row - that would be a self-reference).
    let refresh_hook: Rc<RefCell<Rc<dyn Fn()>>> = Rc::new(RefCell::new(Rc::new(|| {})));
    let refresh_config: Rc<dyn Fn()> = Rc::new({
        let refresh_hook = refresh_hook.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let config_view = config_view.clone();
        // Captured purely so the Edit/Remove closures built below (for
        // manual "other" accounts) can reconnect or tear down a session -
        // everything `connect_other_account` needs, mirroring the set
        // `spawn_account_discovery` already threads through to
        // `connect_account`.
        let worker = worker.clone();
        let keyring = keyring.clone();
        let window = window.clone();
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        let list_header = list_header.clone();
        let dashboard_refresh = dashboard_refresh.clone();
        let mark_read_button = mark_read_button.clone();
        let star_button = star_button.clone();
        // Everything the GOA enable/disable toggle needs beyond the mail
        // set above: the calendar/tasks/contacts wiring, and the page-state
        // cells the re-enable path flips back from their empty states.
        let root_stack = root_stack.clone();
        let current_mail_page = current_mail_page.clone();
        let mail_view_button = mail_view_button.clone();
        let current_calendar_page = current_calendar_page.clone();
        let calendar_view_button = calendar_view_button.clone();
        let current_tasks_page = current_tasks_page.clone();
        let tasks_view_button = tasks_view_button.clone();
        let current_lookout_page = current_lookout_page.clone();
        let lookout_view_button = lookout_view_button.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_sidebar.calendar_list_box.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let reminders_engine = reminders_engine.clone();
        let tasks_view = tasks_view.clone();
        let refresh_contacts_ui = refresh_contacts_ui.clone();
        move || {
            let st = state.borrow();
            // The GOA enable/disable list - every account the discoveries
            // saw, disabled ones included (a disabled account must stay
            // visible here so it can be turned back on).
            let mut goa: Vec<crate::config_view::GoaAccountInfo> = st
                .goa_accounts
                .iter()
                .map(|(account_id, a)| crate::config_view::GoaAccountInfo {
                    display_name: a.display_name.clone(),
                    email: a.email.clone(),
                    account_id: account_id.0.clone(),
                    enabled: st.account_enabled(account_id),
                })
                .collect();
            goa.sort_by_key(|a| a.display_name.to_lowercase());
            let mut mail: Vec<crate::config_view::MailAccountInfo> = st
                .accounts
                .iter()
                .map(|(account_id, h)| crate::config_view::MailAccountInfo {
                    display_name: h.display_name.clone(),
                    email: h.email.clone(),
                    account_id: account_id.0.clone(),
                    is_other: account_id.0.starts_with(crate::app_config::OTHER_ACCOUNT_ID_PREFIX),
                    identity_labels: st
                        .app_config
                        .borrow()
                        .identities_for_account(account_id, &h.display_name, &h.email)
                        .iter()
                        .map(|i| i.label())
                        .collect(),
                    imap: format!("{}:{}", h.imap_host, h.imap_port),
                    smtp: format!("{}:{}", h.smtp_host, h.smtp_port),
                })
                .collect();
            let webcal_list = st.app_config.borrow().webcal_subscriptions.clone();
            drop(st);
            mail.sort_by_key(|a| a.email.to_lowercase());
            let mut calendar: Vec<crate::config_view::CalendarAccountInfo> = calendar_state
                .borrow()
                .accounts
                .values()
                .map(|h| crate::config_view::CalendarAccountInfo {
                    display_name: h.display_name.clone(),
                    uri: h.uri.clone(),
                })
                .collect();
            calendar.sort_by_key(|a| a.display_name.to_lowercase());
            let mut webcal: Vec<crate::config_view::WebcalSubscriptionInfo> = webcal_list
                .iter()
                .map(|sub| crate::config_view::WebcalSubscriptionInfo {
                    display_name: sub.display_name.clone(),
                    url: sub.url.clone(),
                })
                .collect();
            webcal.sort_by_key(|a| a.display_name.to_lowercase());

            let (mail_cache_dir, mail_files) = lookout_mail::cache_info();
            let mail_cache_files: Vec<crate::config_view::CacheFile> = mail_files
                .into_iter()
                .map(|(name, size)| crate::config_view::CacheFile {
                    name,
                    size: crate::config_view::format_size(size),
                })
                .collect();
            let (calendar_cache_dir, cal_files) = lookout_dav::cache_info();
            let calendar_cache_files: Vec<crate::config_view::CacheFile> = cal_files
                .into_iter()
                .map(|(name, size)| crate::config_view::CacheFile {
                    name,
                    size: crate::config_view::format_size(size),
                })
                .collect();
            let (contacts_cache_dir, contacts_files) = lookout_dav::contacts_cache_info();
            let contacts_cache_files: Vec<crate::config_view::CacheFile> = contacts_files
                .into_iter()
                .map(|(name, size)| crate::config_view::CacheFile {
                    name,
                    size: crate::config_view::format_size(size),
                })
                .collect();

            let manage_identities: crate::config_view::ManageIdentities = {
                let state = state.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |anchor, account_id| {
                    let account_id = AccountId(account_id.to_string());
                    let app_config = state.borrow().app_config.clone();
                    let on_changed = {
                        let state = state.clone();
                        let refresh_hook = refresh_hook.borrow().clone();
                        Rc::new(move || {
                            // Any open composer's From dropdown re-reads its
                            // identities live, so a change made in Config
                            // while composing lands in the open list too.
                            if let Some(refresh) = state.borrow().composer_identities_refresh.clone() {
                                refresh();
                            }
                            refresh_hook();
                        })
                    };
                    crate::identities::show_manage_dialog(anchor, app_config, account_id, on_changed);
                })
            };
            let edit_other: crate::config_view::OtherAccountAction = {
                let state = state.clone();
                let worker = worker.clone();
                let keyring = keyring.clone();
                let window = window.clone();
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_button = mark_read_button.clone();
                let star_button = star_button.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |anchor, account_id: &str| {
                    let Some(existing) = state.borrow().app_config.borrow().other_account(account_id) else {
                        return;
                    };
                    let app_config = state.borrow().app_config.clone();
                    let on_saved = {
                        let state = state.clone();
                        let worker = worker.clone();
                        let keyring = keyring.clone();
                        let window = window.clone();
                        let app = app.clone();
                        let toast_overlay = toast_overlay.clone();
                        let folder_selection = folder_selection.clone();
                        let folder_scroller = folder_scroller.clone();
                        let message_list = message_list.clone();
                        let message_list_stack = message_list_stack.clone();
                        let message_header = message_header.clone();
                        let reading_stack = reading_stack.clone();
                        let list_header = list_header.clone();
                        let dashboard_refresh = dashboard_refresh.clone();
                        let mark_read_button = mark_read_button.clone();
                        let star_button = star_button.clone();
                        let refresh_hook = refresh_hook.borrow().clone();
                        Rc::new(move |account_id: &str| {
                            // Editing always fully reconnects: the running
                            // session's `AccountConfig` is immutable once
                            // started, and a full reconnect is cheap and
                            // always correct, unlike diffing which fields
                            // actually changed. Dropping the old handle
                            // closes its command channel, which
                            // `run_account_session` treats as a clean
                            // shutdown request.
                            state.borrow_mut().accounts.remove(&AccountId(account_id.to_string()));
                            let Some(account) = state.borrow().app_config.borrow().other_account(account_id) else {
                                return;
                            };
                            connect_other_account(
                                worker.clone(),
                                state.clone(),
                                folder_selection.clone(),
                                folder_scroller.clone(),
                                message_list.clone(),
                                message_list_stack.clone(),
                                message_header.clone(),
                                reading_stack.clone(),
                                toast_overlay.clone(),
                                window.clone(),
                                app.clone(),
                                list_header.clone(),
                                account,
                                keyring.clone(),
                                dashboard_refresh.clone(),
                                mark_read_button.clone(),
                                star_button.clone(),
                            );
                            refresh_hook();
                        })
                    };
                    crate::other_accounts::show_add_account_dialog(anchor, worker.clone(), app_config, keyring.clone(), Some(existing), on_saved);
                })
            };
            let remove_other: crate::config_view::OtherAccountAction = {
                let state = state.clone();
                let worker = worker.clone();
                let keyring = keyring.clone();
                let toast_overlay = toast_overlay.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |_anchor, account_id: &str| {
                    let account_id = AccountId(account_id.to_string());
                    state.borrow_mut().accounts.remove(&account_id);
                    {
                        let app_config = state.borrow().app_config.clone();
                        app_config.borrow_mut().other_accounts.retain(|a| a.id != account_id.0);
                        crate::app_config::save(&app_config.borrow());
                    }
                    let keyring = keyring.clone();
                    let account_id_for_worker = account_id.clone();
                    worker.spawn(async move {
                        let _ = keyring.delete_account(&account_id_for_worker).await;
                    });
                    if let Err(e) = lookout_mail::remove_account_cache(&account_id) {
                        tracing::warn!("could not remove cache for removed account {account_id}: {e}");
                    }
                    toast_overlay.add_toast(adw::Toast::new("Account removed"));
                    let refresh = refresh_hook.borrow().clone();
                    refresh();
                })
            };
            // The GOA enable/disable switches: persist the preference, then
            // connect the account's services back up (enable) or tear them
            // down and hide everything (disable).
            let toggle_goa: crate::config_view::AccountToggle = {
                let state = state.clone();
                let calendar_state = calendar_state.clone();
                let worker = worker.clone();
                let window = window.clone();
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_button = mark_read_button.clone();
                let star_button = star_button.clone();
                let root_stack = root_stack.clone();
                let current_mail_page = current_mail_page.clone();
                let mail_view_button = mail_view_button.clone();
                let current_calendar_page = current_calendar_page.clone();
                let calendar_view_button = calendar_view_button.clone();
                let current_tasks_page = current_tasks_page.clone();
                let tasks_view_button = tasks_view_button.clone();
                let current_lookout_page = current_lookout_page.clone();
                let lookout_view_button = lookout_view_button.clone();
                let calendar_main = calendar_main.clone();
                let calendar_list_box = calendar_list_box.clone();
                let mini_calendar = mini_calendar.clone();
                let mail_overview_day = mail_overview_day.clone();
                let mail_overview_day_list = mail_overview_day_list.clone();
                let reminders_engine = reminders_engine.clone();
                let tasks_view = tasks_view.clone();
                let refresh_contacts_ui = refresh_contacts_ui.clone();
                let refresh_hook = refresh_hook.clone();
                Rc::new(move |account_id: &str, enabled: bool| {
                    let id = AccountId(account_id.to_string());
                    state.borrow().set_account_enabled(&id, enabled);
                    if enabled {
                        let discovered = state.borrow().goa_accounts.get(&id).cloned();
                        if let Some(discovered) = discovered {
                            connect_goa_account(
                                worker.clone(),
                                state.clone(),
                                calendar_state.clone(),
                                &id,
                                &discovered,
                                root_stack.clone(),
                                current_mail_page.clone(),
                                mail_view_button.clone(),
                                current_calendar_page.clone(),
                                calendar_view_button.clone(),
                                current_tasks_page.clone(),
                                tasks_view_button.clone(),
                                current_lookout_page.clone(),
                                lookout_view_button.clone(),
                                folder_selection.clone(),
                                folder_scroller.clone(),
                                message_list.clone(),
                                message_list_stack.clone(),
                                message_header.clone(),
                                reading_stack.clone(),
                                toast_overlay.clone(),
                                window.clone(),
                                app.clone(),
                                list_header.clone(),
                                dashboard_refresh.clone(),
                                mark_read_button.clone(),
                                star_button.clone(),
                                calendar_main.clone(),
                                calendar_list_box.clone(),
                                mini_calendar.clone(),
                                mail_overview_day.clone(),
                                mail_overview_day_list.clone(),
                                reminders_engine.clone(),
                                tasks_view.clone(),
                                refresh_contacts_ui.clone(),
                            );
                        }
                    } else {
                        let email = state.borrow().goa_accounts.get(&id).map(|a| a.email.clone());
                        teardown_goa_account(
                            &state,
                            &calendar_state,
                            &id,
                            email.as_deref(),
                            &folder_selection,
                            &folder_scroller,
                            &message_list,
                            &message_list_stack,
                            &list_header,
                            &calendar_main,
                            &calendar_list_box,
                            &reminders_engine,
                            &tasks_view,
                            &refresh_contacts_ui,
                            &dashboard_refresh,
                        );
                    }
                    // Repaint the Config account list so the switch's new
                    // state and the service lists underneath stay current.
                    let refresh = refresh_hook.borrow().clone();
                    refresh();
                })
            };
            crate::config_view::refresh(
                &config_view,
                &goa,
                &mail,
                &calendar,
                &webcal,
                &mail_cache_dir,
                &mail_cache_files,
                &calendar_cache_dir,
                &calendar_cache_files,
                &contacts_cache_dir,
                &contacts_cache_files,
                &manage_identities,
                &edit_other,
                &remove_other,
                &toggle_goa,
            );
        }
    });
    // From here on, the manage-identities dialog's `on_changed` re-runs this
    // closure (and refreshes any open composer's From dropdown via
    // `composer_identities_refresh`), keeping the Config view's rows current
    // after an edit. (The `Rc` cycle this creates lives for the window's
    // lifetime, like the other closure cycles this file already accepts.)
    *refresh_hook.borrow_mut() = refresh_config.clone();
    // Populate the placeholder rows now (both groups are empty at startup).
    refresh_config();

    // Config → Accounts → "Add IMAP/SMTP account…": the app's own manual
    // account entry point, alongside the GOA-settings "Add account…" row.
    {
        let add_imap_row = config_view.add_imap_row.clone();
        let worker = worker.clone();
        let state = state.clone();
        let keyring = keyring.clone();
        let window = window.clone();
        let app = app.clone();
        let toast_overlay = toast_overlay.clone();
        let folder_selection = folder_selection.clone();
        let folder_scroller = folder_scroller.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        let list_header = list_header.clone();
        let dashboard_refresh = dashboard_refresh.clone();
        let mark_read_button = mark_read_button.clone();
        let star_button = star_button.clone();
        let refresh_hook = refresh_hook.clone();
        add_imap_row.connect_activated(move |row| {
            let app_config = state.borrow().app_config.clone();
            let on_saved = {
                let state = state.clone();
                let worker = worker.clone();
                let keyring = keyring.clone();
                let window = window.clone();
                let app = app.clone();
                let toast_overlay = toast_overlay.clone();
                let folder_selection = folder_selection.clone();
                let folder_scroller = folder_scroller.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                let message_header = message_header.clone();
                let reading_stack = reading_stack.clone();
                let list_header = list_header.clone();
                let dashboard_refresh = dashboard_refresh.clone();
                let mark_read_button = mark_read_button.clone();
                let star_button = star_button.clone();
                let refresh_hook = refresh_hook.borrow().clone();
                Rc::new(move |account_id: &str| {
                    let Some(account) = state.borrow().app_config.borrow().other_account(account_id) else {
                        return;
                    };
                    connect_other_account(
                        worker.clone(),
                        state.clone(),
                        folder_selection.clone(),
                        folder_scroller.clone(),
                        message_list.clone(),
                        message_list_stack.clone(),
                        message_header.clone(),
                        reading_stack.clone(),
                        toast_overlay.clone(),
                        window.clone(),
                        app.clone(),
                        list_header.clone(),
                        account,
                        keyring.clone(),
                        dashboard_refresh.clone(),
                        mark_read_button.clone(),
                        star_button.clone(),
                    );
                    refresh_hook();
                })
            };
            crate::other_accounts::show_add_account_dialog(row.upcast_ref::<gtk::Widget>(), worker.clone(), app_config, keyring.clone(), None, on_saved);
        });
    }

    {
        let root_stack = root_stack.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let refresh_config = refresh_config.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        config_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("config");
                root_stack.set_visible_child_name("config");
                view_toolbar_stack.set_visible_child_name("config");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
                refresh_config();
            }
        });
    }

    // --- "Clear all caches" (Config → Advanced): deletes the on-disk mail,
    // calendar and contacts caches, drops the in-memory calendar occurrences,
    // and asks every connected account to resync so the caches rebuild from
    // the servers right away rather than on next launch ---
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let toast_overlay = toast_overlay.clone();
        config_view.clear_cache_row.connect_activated(move |_| {
            match (lookout_mail::clear_all_caches(), lookout_dav::clear_all_caches()) {
                (Ok(()), Ok(())) => toast_overlay.add_toast(adw::Toast::new("Cleared email, calendar and contacts caches")),
                (Err(e), _) => {
                    let title = glib::markup_escape_text(&format!("Couldn't clear caches: {e}"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
                (_, Err(e)) => {
                    let title = glib::markup_escape_text(&format!("Couldn't clear caches: {e}"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
            }
            let month = calendar_state.borrow().displayed_month;
            for handle in calendar_state.borrow_mut().accounts.values_mut() {
                handle.last_occurrences.clear();
                handle.last_synced_month = None;
                handle.occurrences_by_month.clear();
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
            }
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
            refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
            for handle in state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::Refresh);
            }
            for cmd_tx in state.borrow().contact_cmd_tx.values() {
                let _ = cmd_tx.send_blocking(ContactCommand::Refresh);
            }
        });
    }

    // --- Config → Google Tasks → OAuth client id: shows the configured id
    // (or its absence) and opens a small entry dialog to set/clear it - the
    // GUI-friendly way to provide the client id Google requires, instead of
    // an environment variable.
    {
        let window = window.clone();
        let toast_overlay = toast_overlay.clone();
        let google_tasks_client_row = config_view.google_tasks_client_row.clone();
        crate::config_view::refresh_google_tasks_client_row(&google_tasks_client_row);
        let row_for_dialog = google_tasks_client_row.clone();
        google_tasks_client_row.connect_activated(move |_| {
            show_google_tasks_client_id_dialog(&window, &toast_overlay, &row_for_dialog);
        });
    }

    // --- Compose button -> new-message composer in the reading pane,
    // "From" = the account owning the selected message (falling back to the
    // currently-open mailbox's account, then any connected account) ---
    {
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        let message_list = message_list.clone();
        compose_button.connect_clicked(move |_| {
            let st = state.borrow();
            // A section header is selected exactly like "nothing selected"
            // here: fall through to the open mailbox's account.
            let account_id = message_list
                .selected_summary()
                .and_then(|summary| mailbox_account_id(&summary.mailbox))
                .or_else(|| st.current_account.clone())
                .or_else(|| st.accounts.keys().next().cloned());
            let Some(handle) = account_id.clone().and_then(|id| st.accounts.get(&id)) else { return };
            let cmd_tx = handle.cmd_tx.clone();
            let rich_text_default = state.borrow().rich_text_default;
            drop(st);
            show_composer_in_reading_pane(
                &state,
                &worker,
                &reading_stack,
                "New Message",
                cmd_tx,
                crate::compose::ComposePrefill::default(),
                rich_text_default,
                account_id,
            );
        });
    }

    // --- Reply/Reply-All/Forward -> opens the composer in the reading pane
    // pre-filled from whatever message is currently selected and has a body
    // loaded. Silent no-op if nothing's selected or the body hasn't arrived
    // yet (same convention as the Delete/Archive/Report/Snooze buttons below).
    // Wired to three copies of each button - the top command toolbar, the
    // reading-pane header, and the bottom action bar below the message body -
    // by design, see the plan this shipped under.
    for (button, mode, title) in [
        (&reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&message_header.reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&message_header.bottom_reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
        (&message_header.reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
    ] {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        button.connect_clicked(move |_| {
            if let Some((summary, body, from_email, cmd_tx)) = selected_message_reply_context(&message_list, &state) {
                let prefill = crate::compose::build_reply_prefill(&summary, &body, &from_email, mode);
                let rich_text_default = state.borrow().rich_text_default;
                show_composer_in_reading_pane(&state, &worker, &reading_stack, title, cmd_tx, prefill, rich_text_default, mailbox_account_id(&summary.mailbox));
            }
        });
    }
    for button in [&forward_button, &message_header.forward_button, &message_header.bottom_forward_button] {
        let message_list = message_list.clone();
        let state = state.clone();
        let worker = worker.clone();
        let reading_stack = reading_stack.clone();
        button.connect_clicked(move |_| {
            if let Some((summary, body, _from_email, cmd_tx)) = selected_message_reply_context(&message_list, &state) {
                let prefill = crate::compose::build_forward_prefill(&summary, &body);
                let rich_text_default = state.borrow().rich_text_default;
                show_composer_in_reading_pane(&state, &worker, &reading_stack, "Forward", cmd_tx, prefill, rich_text_default, mailbox_account_id(&summary.mailbox));
            }
        });
    }

    // --- "View contact" (reading-pane header): looks the selected message's
    // sender up across every connected contacts account. Found - the contact
    // editor opens for that card (edit mode, routed to its own account).
    // Not found - the create editor opens prefilled with the sender, its
    // address-book picker listing every account's books with the account the
    // email lives in first. Silent no-op when nothing's selected or the
    // message has no sender.
    {
        let window = window.clone();
        let state = state.clone();
        let toast_overlay = toast_overlay.clone();
        let message_list = message_list.clone();
        message_header.contact_button.connect_clicked(move |_| {
            let summary = match message_list.selected_summary() {
                Some(summary) => summary,
                None => return,
            };
            let Some(sender) = summary.from.first() else { return };
            let account_id = mailbox_account_id(&summary.mailbox);
            match find_contact_by_address(&state, &sender.address, account_id.as_ref()) {
                Some((_account_id, entry)) => show_contact_editor_for(&window, &state, &toast_overlay, &entry),
                None => show_create_contact_for(&window, &state, &toast_overlay, sender, account_id),
            }
        });
    }

    // --- "Switch message theme" toggle: arms/removes the background-stripping
    // user stylesheet and re-renders the open message so the override applies
    // to it, not just the next selection. A per-email override: every
    // navigation reset sets `message_theme_override` to the Config →
    // Appearance "Dark message theme" default (see `set_message_theme_armed`
    // for the physical side), and `render_body` syncs this button from it,
    // so the next message always opens in the configured default rather than
    // inheriting the previous message's manual override. The idempotence
    // guard up front makes the handler a no-op when state and button already
    // agree - which is what happens when `render_body`'s sync flips the
    // button after a navigation, so that sync-driven `toggled` can't trigger
    // a redundant re-render (or a loop). Re-rendering is skipped while a
    // composer is up, the same as the "Load images" toggle - see
    // `rerender_current_message`. Stripping the email's backgrounds alone is
    // not enough: the WebView's own page canvas paints white behind the (now
    // transparent) document, so the toggle also flips the canvas colour -
    // transparent while armed (letting the reading card's theme background
    // show through), white otherwise, matching WebKit's default so
    // un-backgrounded emails look exactly as they did before the toggle
    // existed.
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        let message_list = message_list.clone();
        let web_view = web_view.clone();
        let user_content_manager = user_content_manager.clone();
        let theme_override_sheet = theme_override_sheet.clone();
        message_header.theme_button.clone().connect_toggled(move |button| {
            // The icon flips with the state - a moon while the override is
            // off, a sun while it's on (the sun also trails the moon's
            // candidate list as the no-moon fallback, see `message_header`).
            // The tooltip names what clicking the button will do next, so it
            // flips too - and both are set before the guard below so
            // `render_body`'s sync-driven `set_active` (which also lands
            // here) updates them.
            button.set_icon_name(if button.is_active() {
                crate::window::themed_icon_name(&["weather-clear-symbolic", "display-brightness-symbolic"])
            } else {
                crate::window::themed_icon_name(&["weather-clear-night-symbolic", "night-light-symbolic", "weather-clear-symbolic"])
            });
            button.set_tooltip_text(Some(if button.is_active() { "View in Light Mode" } else { "View in Dark Mode" }));
            if button.is_active() == state.borrow().message_theme_override {
                return;
            }
            set_message_theme_armed(button.is_active(), &web_view, &user_content_manager, &theme_override_sheet);
            state.borrow_mut().message_theme_override = button.is_active();
            rerender_current_message(&state, &reading_stack, &message_header, &message_list);
        });
    }

    // --- Debug: open a raw .eml fixture straight into the reading pane ---
    #[cfg(debug_assertions)]
    {
        let window = window.clone();
        let state = state.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        open_eml_button.connect_clicked(move |_| {
            let window = window.clone();
            let state = state.clone();
            let message_header = message_header.clone();
            let reading_stack = reading_stack.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("eml");
                filter.set_name(Some("Email messages (*.eml)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);

                let dialog = gtk::FileDialog::builder().title("Open .eml (debug)").filters(&filters).build();

                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                let Ok(raw) = std::fs::read(&path) else { return };
                if let Some(body) = lookout_mail::body::parse_body(lookout_core::Uid(0), &raw) {
                    // The debug viewer has no IMAP session behind it, so the
                    // reading pane's cid: scheme handler can't fetch inline
                    // images - rewrite them to `data:` URIs straight from the
                    // raw message so fixtures render in full.
                    let body = if let Some(html) = body.html_body.as_deref() {
                        lookout_core::EmailBody {
                            html_body: Some(lookout_mail::body::rewrite_cid_refs_to_data_uris(html, &raw)),
                            ..body
                        }
                    } else {
                        body
                    };
                    render_body(&state, &reading_stack, &message_header, MailboxId("debug:eml".into()), body.uid, body);
                }
            });
        });
    }

    // --- Network connectivity -> nudge every backed-off account to retry now ---
    {
        let state = state.clone();
        let monitor = gio::NetworkMonitor::default();
        monitor.connect_network_changed(move |_monitor, available| {
            if !available {
                return;
            }
            for handle in state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::Reconnect);
            }
        });
    }

    // --- Folder selection -> AccountCommand::SyncMailbox on that folder's
    // own account (selecting an account-group row, or the Favorites section
    // header, is a no-op - it just expands/collapses). The synthetic "All
    // Inboxes" row instead enters the unified view. A `Favorite` row is the
    // same mailbox as its `Folder` row, so it takes the identical path. Every
    // live path re-renders the list header, which is the one place both view
    // changes funnel through. ---
    {
        let state = state.clone();
        let worker = worker.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        let list_header = list_header.clone();
        let search_entry = search_entry.clone();
        folder_selection.connect_selected_item_notify(move |sel| {
            // A rebuild putting the highlight back where it was is not the
            // user navigating; see `UiState::suppress_folder_selection`.
            if state.borrow().suppress_folder_selection {
                return;
            }
            let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let tree_item = boxed.borrow::<TreeItem>();
            match &*tree_item {
                TreeItem::Unified(_) => {
                    drop(tree_item);
                    // Clicking a folder is a deliberate navigation away from
                    // search; leave it and let the selection take over.
                    exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, &search_entry);
                    enter_unified_inbox(&state, &message_list, &message_list_stack);
                    refresh_list_header(&state, &list_header);
                }
                TreeItem::Folder(node) | TreeItem::Favorite(node) => {
                    let mailbox_id = node.mailbox.id.clone();
                    let account_id = node.mailbox.account_id.clone();
                    drop(tree_item);
                    exit_search(&state, &worker, &message_list, &message_list_stack, &list_header, &search_entry);
                    select_mailbox(&state, &worker, &message_list, &message_list_stack, account_id, mailbox_id);
                    refresh_list_header(&state, &list_header);
                }
                TreeItem::Account(_) | TreeItem::Favorites => {}
            }
        });
    }

    // --- Message selection -> AccountCommand::FetchBody on the current account ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let reading_multi_label = reading_multi_label.clone();
        let message_header = message_header.clone();
        let message_list_for_selection = message_list.clone();
        let mark_read_button = mark_read_button.clone();
        let star_button = star_button.clone();
        let task_button = task_button.clone();
        let calendar_state = calendar_state.clone();
        let web_view = web_view.clone();
        let user_content_manager = user_content_manager.clone();
        let theme_override_sheet = theme_override_sheet.clone();
        message_list.selection.connect_selection_changed(move |_sel, pos, n_items| {
            let kind = match message_list_for_selection.selection_kind() {
                SelectionKind::Empty => "Empty",
                SelectionKind::Section => "Section",
                SelectionKind::Message(_) => "Message",
                SelectionKind::Multiple(_) => "Multiple",
            };
            tracing::debug!(pos, n_items, kind, "selection-changed fired");
            refresh_mark_read_button(&mark_read_button, &message_list_for_selection);
            refresh_star_button(&star_button, &message_list_for_selection);
            refresh_task_button(&task_button, &message_list_for_selection, &calendar_state);
            let summary = match message_list_for_selection.selection_kind() {
                SelectionKind::Message(summary) => *summary,
                // A date section header - unreachable via the mouse (headers
                // are unselectable, see `bind()`), kept as a defensive no-op.
                SelectionKind::Section => return,
                SelectionKind::Multiple(summaries) => {
                    // Two or more messages selected: show the "N selected"
                    // placeholder and skip mark-as-read/body-fetch entirely -
                    // the same state reset the `Empty` arm below does, since
                    // both mean "the reading pane no longer shows a specific
                    // message." Leaving `rendered_message` set here would
                    // make a later re-selection of that same single message
                    // look already-shown and skip re-rendering it. The
                    // message-theme override resets to the configured default
                    // (Config → Appearance → "Dark message theme") and its
                    // physical sheet/canvas are re-armed to match.
                    let default_dark = {
                        let mut st = state.borrow_mut();
                        st.pending_body_request = None;
                        st.pending_html_reveal = false;
                        st.reveal_generation += 1;
                        st.pending_header = None;
                        st.pending_attachment = None;
                        st.pending_raw_message = None;
                        st.unsubscribe_info = None;
                        st.unsubscribe_dismissed = None;
                        st.imip = None;
                        st.imip_dismissed = None;
                        st.read_receipt_request = None;
                        st.read_receipt_dismissed = None;
                        st.read_receipt_context = None;
                        st.rendered_trust_sender = None;
                        st.load_once_images = false;
                        st.message_theme_override = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                        st.trust_banner_dismissed = None;
                        st.rendered_message = None;
                        st.message_theme_override
                    };
                    drop_pending_cid(&state);
                    set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
                    reading_multi_label.set_label(&format!("{} messages selected", summaries.len()));
                    reading_stack.set_visible_child_name("multi");
                    return;
                }
                SelectionKind::Empty => {
                    // Nothing selected: show the "no message selected"
                    // placeholder and drop every per-message state - but only
                    // once this is confirmed to persist. A list repopulate's
                    // splice can transiently shrink the selection to nothing
                    // before `restore_selection` (moments later, same
                    // synchronous call) puts it right back on the same
                    // message - acting on that blip immediately would clear
                    // `pending_body_request` before the `Message` arm's
                    // re-fire ever gets a chance to recognize it as the same
                    // request (see that arm's same-request guard), stranding
                    // any fetch or WebKit load already in flight for it.
                    // Deferring to the next main-loop idle callback lets that
                    // resettle first; the reset below only actually applies
                    // if the selection is still empty by then.
                    let state = state.clone();
                    let message_list_for_selection = message_list_for_selection.clone();
                    let web_view = web_view.clone();
                    let user_content_manager = user_content_manager.clone();
                    let theme_override_sheet = theme_override_sheet.clone();
                    let reading_stack = reading_stack.clone();
                    glib::idle_add_local_once(move || {
                        if !matches!(message_list_for_selection.selection_kind(), SelectionKind::Empty) {
                            return;
                        }
                        let default_dark = {
                            let mut st = state.borrow_mut();
                            st.pending_body_request = None;
                            st.pending_html_reveal = false;
                            st.reveal_generation += 1;
                            st.pending_header = None;
                            st.pending_attachment = None;
                            st.pending_raw_message = None;
                            st.unsubscribe_info = None;
                            st.unsubscribe_dismissed = None;
                            st.imip = None;
                            st.imip_dismissed = None;
                            st.read_receipt_request = None;
                            st.read_receipt_dismissed = None;
                            st.read_receipt_context = None;
                            st.rendered_trust_sender = None;
                            st.load_once_images = false;
                            st.message_theme_override = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                            st.trust_banner_dismissed = None;
                            st.rendered_message = None;
                            st.message_theme_override
                        };
                        drop_pending_cid(&state);
                        set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
                        reading_stack.set_visible_child_name("empty");
                    });
                    return;
                }
            };
            let uid = summary.uid;
            let mailbox = summary.mailbox.clone();
            // Re-selecting the message that's already on the reading pane -
            // which `restore_selection` can do across a rebuild that
            // preserves the same selected row - must be a no-op, not a fresh
            // fetch/render. Routing it through "empty" and crossfading the
            // same email back in would be a startup flicker; the body is
            // already on screen, so there's nothing to re-render.
            let already_shown = {
                let st = state.borrow();
                st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid)) && reading_stack.visible_child_name().as_deref() == Some("message")
            };
            if already_shown {
                return;
            }
            let request = (mailbox.clone(), uid);
            // Opening a message is what marks it read - bodies are fetched
            // with `BODY.PEEK`, so the server never sets `\Seen` itself (see
            // `fetch_body` in lookout-mail). Sent past the `already_shown`
            // guard above, so the rebuild this triggers can't loop: the
            // message comes back with `\Seen` set, and a re-selection of the
            // row already on the reading pane returns before reaching here.
            // Bulwark's configurable mark-as-read delay is a later refinement;
            // this marks on open, as Outlook's default reading pane does.
            let mark_read = summary.is_unread();
            // Deferred to the reveal: updating the header here would swap it
            // to the next email while the previous message is still fading
            // out. `render_body` applies it once the new body is about to be
            // shown (the pane is on "empty" by then, so it can't flash).
            state.borrow_mut().pending_header = Some(summary);
            if mark_read {
                let st = state.borrow();
                if let Some(handle) = mailbox_account_id(&mailbox).and_then(|id| st.accounts.get(&id)) {
                    let _ = handle.cmd_tx.send_blocking(AccountCommand::StoreFlags {
                        mailbox: mailbox.clone(),
                        uid,
                        add: vec![SystemFlagBit::Seen],
                        remove: Vec::new(),
                    });
                }
            }

            let body_is_cached = state.borrow_mut().body_cache.get(&mailbox, &uid).is_some();
            let should_request = {
                let mut st = state.borrow_mut();
                let is_same_request = st.pending_body_request.as_ref() == Some(&request);
                // Disarm any body load still in flight for the previously
                // selected message, so its `Finished` can't reveal a stale
                // email once the user has moved on. The reveal-fallback
                // timeouts capture `reveal_generation` at arm time, so the
                // bump here also invalidates any timeout from an earlier
                // selection whose load hasn't finished yet.
                //
                // Only when the selection is actually changing to a
                // *different* message, though: this handler can re-fire for
                // the message that's already selected - a list repaint
                // (e.g. the resync that follows marking this very message
                // read) can transiently touch its row during a splice, which
                // `restore_selection` corrects moments later, but not before
                // this signal fires again for the same `(mailbox, uid)`.
                // Disarming on that re-fire would strand the pane on "empty"
                // forever - WebKit only fires `Finished` once per load, so a
                // load that's still genuinely in flight for this exact
                // message must not be cancelled here.
                if !is_same_request {
                    st.pending_html_reveal = false;
                    st.reveal_generation += 1;
                    st.pending_body_request = Some(request.clone());
                }
                !body_is_cached && !is_same_request
            };
            if should_request {
                let st = state.borrow();
                if let Some(account_id) = mailbox_account_id(&mailbox) {
                    if let Some(handle) = st.accounts.get(&account_id) {
                        tracing::debug!(?mailbox, uid = uid.0, "FetchBody: dispatching to account actor");
                        let _ = handle.cmd_tx.send_blocking(AccountCommand::FetchBody { mailbox: mailbox.clone(), uid });
                    }
                }
            }
            // Also silently abandons an in-progress composer in the reading
            // pane, if one's open - no "discard draft?" prompt, consistent
            // with this app's existing no-confirmation-dialog convention.
            reading_stack.set_visible_child_name("empty");
            // Every navigation resets the per-message override to the
            // configured default (Config → Appearance → "Dark message
            // theme"), so the next message opens in that default rather than
            // inheriting the previous message's manual toggle - the physical
            // sheet/canvas are re-armed to match before the new body renders
            // (or while the pane sits on "empty" waiting for a fetch).
            {
                let mut st = state.borrow_mut();
                let default_dark = st.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK);
                st.message_theme_override = default_dark;
                drop(st);
                set_message_theme_armed(default_dark, &web_view, &user_content_manager, &theme_override_sheet);
            }
            if body_is_cached {
                let body = state.borrow_mut().body_cache.get(&mailbox, &uid);
                if let Some(body) = body {
                    tracing::debug!(?mailbox, uid = uid.0, "FetchBody: serving from in-memory cache");
                    render_body(&state, &reading_stack, &message_header, mailbox, uid, body);
                }
            }
        });
    }

    // --- Delete/Archive/Report -> AccountCommand::MoveMessages against the
    // account's Trash/Archive/Junk mailbox; Snooze -> AccountCommand::
    // SnoozeMessages with a single fixed "tomorrow 9:00 AM local time"
    // default applied to the whole selection. All four are silent no-ops
    // with nothing selected, and send exactly one command per distinct
    // mailbox in the selection (see `selected_message_command_targets`) -
    // for today's ordinary single-selection case that's exactly one command,
    // same as before.
    for (button, role) in [
        (&delete_button, MailboxRole::Trash),
        (&archive_button, MailboxRole::Archive),
        (&report_button, MailboxRole::Junk),
    ] {
        let message_list = message_list.clone();
        let state = state.clone();
        button.connect_clicked(move |_| {
            for (cmd_tx, mailbox, uids) in selected_message_command_targets(&message_list, &state) {
                optimistic_remove_messages(&state, &message_list, &mailbox, &uids);
                let _ = cmd_tx.send_blocking(AccountCommand::MoveMessages { mailbox, uids, role });
            }
        });
    }
    // --- Star/Unstar -> AccountCommand::StoreFlagsMany toggling `\Flagged`
    // on every selected message. The direction is computed once over the
    // whole selection - Gmail/Outlook's convention: any unflagged message
    // selected means the action flags everything; only when every selected
    // message is already flagged does it become "unflag all" - rather than
    // per-message, so a single selected message (the common case) sees
    // exactly today's is_starred()-based toggle.
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let star_button_for_click = star_button.clone();
        star_button.connect_clicked(move |_| {
            let summaries = message_list.selected_summaries();
            if summaries.is_empty() {
                return;
            }
            let (add, remove) = if summaries.iter().any(|s| !s.is_starred()) {
                (vec![SystemFlagBit::Flagged], Vec::new())
            } else {
                (Vec::new(), vec![SystemFlagBit::Flagged])
            };
            // Flag changes aren't patched into the message list model like
            // Mark Read's are (see `restore_optimistic_flag_changes`'s
            // comment), so the button flips its own icon immediately rather
            // than waiting for the server's `MessagesUpdated` confirmation.
            star_button_for_click.set_icon_name(star_icon_name(!add.is_empty()));
            for (cmd_tx, mailbox, uids) in selected_message_command_targets(&message_list, &state) {
                let _ = cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
                    mailbox,
                    uids,
                    add: add.clone(),
                    remove: remove.clone(),
                });
            }
        });
    }
    // --- Add as Task: opens the task editor prefilled from the selected
    // message's subject (title) and sender/date (Notes - `CalendarTask` has
    // no url/link field, so this is the only way back to the source email),
    // defaulting the calendar/list picker to the email's own account when it
    // has a task-capable one. Silent no-op with nothing selected or a
    // multi-selection - `selected_summary()` is `None` for both, same
    // convention as the reading pane's "View contact" button.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let message_list = message_list.clone();
        task_button.connect_clicked(move |_| {
            let Some(summary) = message_list.selected_summary() else { return };
            let account_id = mailbox_account_id(&summary.mailbox);
            let account_email = account_id.as_ref().and_then(|id| state.borrow().accounts.get(id).map(|h| h.email.clone()));
            show_create_task_for_email(&window, &calendar_state, &tasks_view, &summary, account_id, account_email);
        });
    }
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let mark_read_button_for_click = mark_read_button.clone();
        mark_read_button.connect_clicked(move |_| {
            let summaries = message_list.selected_summaries();
            if summaries.is_empty() {
                return;
            }
            let mark_read = summaries.iter().any(|s| s.is_unread());
            let (add, remove) = if mark_read {
                (vec![SystemFlagBit::Seen], Vec::new())
            } else {
                (Vec::new(), vec![SystemFlagBit::Seen])
            };
            for (cmd_tx, mailbox, uids) in selected_message_command_targets(&message_list, &state) {
                optimistic_toggle_read(&state, &message_list, &mailbox, &uids, mark_read);
                let _ = cmd_tx.send_blocking(AccountCommand::StoreFlagsMany {
                    mailbox,
                    uids,
                    add: add.clone(),
                    remove: remove.clone(),
                });
            }
            refresh_mark_read_button(&mark_read_button_for_click, &message_list);
        });
    }
    {
        let message_list = message_list.clone();
        let state = state.clone();
        snooze_button.connect_clicked(move |_| {
            let targets = selected_message_command_targets(&message_list, &state);
            if targets.is_empty() {
                return;
            }
            let tomorrow_9am = chrono::Local::now()
                .date_naive()
                .succ_opt()
                .and_then(|d| d.and_hms_opt(9, 0, 0))
                .and_then(|dt| dt.and_local_timezone(chrono::Local).single())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now);
            for (cmd_tx, mailbox, uids) in targets {
                let _ = cmd_tx.send_blocking(AccountCommand::SnoozeMessages {
                    mailbox,
                    uids,
                    until: tomorrow_9am,
                });
            }
        });
    }

    // --- Main calendar navigation: prev/next/Today move the anchor by the
    // active view's natural unit (day/week/month - see `calendar_view::step`)
    // and redraw every view immediately, then ask every connected calendar
    // account to resync the newly-displayed month.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let prev_button = calendar_main.prev_button.clone();
        prev_button.connect_clicked(move |_| {
            calendar_view::step(&calendar_main, -1);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let next_button = calendar_main.next_button.clone();
        next_button.connect_clicked(move |_| {
            calendar_view::step(&calendar_main, 1);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let today_button = calendar_main.today_button.clone();
        today_button.connect_clicked(move |_| {
            calendar_view::go_today(&calendar_main);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Sidebar mini-calendar -> re-anchor the main panel to the clicked
    // date (whatever view is active) and resync that date's month.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        calendar_view::connect_day_selected(&calendar_sidebar.mini_calendar, move |date| {
            calendar_view::set_anchor(&calendar_main, date);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Mail-screen overview mini-calendar -> re-show that day's events
    // (from whatever's already cached) and ask every connected calendar
    // account to resync that month in the background, without touching the
    // main Calendar view's own `displayed_month`.
    {
        let calendar_state = calendar_state.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        calendar_view::connect_day_selected(&mail_calendar_overview, move |date| {
            mail_overview_day.set(date);
            refresh_mail_overview_day_list(&calendar_state, date, &mail_overview_day_list);
            let month = first_of_month(date);
            for handle in calendar_state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
            }
            let webcal_cmd = calendar_state.borrow().webcal_cmd_tx.clone();
            if let Some(cmd) = webcal_cmd {
                let _ = cmd.send_blocking(SubscriptionCommand::SyncMonth(month));
            }
        });
    }

    // --- Calendar event editor: the "New Event" toolbar button opens a blank
    // form prefilled for the displayed date (and the Day/Week grids' empty
    // time slots open one prefilled for the clicked slot - see the
    // `connect_slot_activated` block below); clicking an event in any calendar
    // view opens it for editing/deleting. Both save and delete hand the
    // result to the owning account's session (`route_calendar_save`/`_delete`),
    // which PUTs/DELETEs it and resyncs the month - the UI repaints through the
    // existing `OccurrencesUpdated` path, so there's no refresh work here.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let toast_overlay = toast_overlay.clone();
        new_event_button.connect_clicked(move |_| {
            let anchor = calendar_view::anchor(&calendar_main);
            let suggested_start = {
                let now = chrono::Local::now();
                if now.date_naive() == anchor {
                    if now.hour() == 23 {
                        (anchor + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap()
                    } else {
                        anchor.and_hms_opt(now.hour() + 1, 0, 0).unwrap()
                    }
                } else {
                    anchor.and_hms_opt(9, 0, 0).unwrap()
                }
            };
            show_new_event_editor(&window, &state, &worker, &calendar_state, &toast_overlay, suggested_start, None);
        });
    }
    // Print: snapshots the currently-displayed month (checked calendars
    // only, matching the view) as an agenda and sends it through WebKit's
    // print pipeline.
    {
        let window = window.clone();
        let calendar_state = calendar_state.clone();
        print_button.connect_clicked(move |_| {
            print_calendar_month(&calendar_state, &window);
        });
    }
    // --- Task editor + list wiring: the "New Task" toolbar button opens a
    // blank form, clicking a task row opens it for editing, and the row
    // checkbox flips completion (the modified task goes through the same
    // session route as a Save). Opening the Tasks view pulls a fresh task
    // list from every source so the view isn't stale between polls.
    {
        let window = window.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        new_task_button.connect_clicked(move |_| {
            show_new_task_editor(&window, &calendar_state, &tasks_view);
        });
    }
    {
        let window = window.clone();
        let calendar_state = calendar_state.clone();
        let calendar_state_for_activate = calendar_state.clone();
        let tasks_view_for_toggle = tasks_view.clone();
        let tasks_view_for_activate = tasks_view.clone();
        crate::tasks_view::set_handlers(
            &tasks_view,
            Rc::new(move |task, completed| route_task_toggle(&calendar_state, &tasks_view_for_toggle, task, completed)),
            Rc::new(move |task| open_task_editor_for(&window, &calendar_state_for_activate, &tasks_view_for_activate, &task)),
        );
    }
    {
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        tasks_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                refresh_tasks_view(&calendar_state, &tasks_view);
                for handle in calendar_state.borrow().accounts.values() {
                    let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncTasks);
                }
                for handle in calendar_state.borrow().google_tasks.values() {
                    let _ = handle.cmd_tx.send_blocking(GoogleTasksCommand::Refresh);
                }
            }
        });
    }
    // --- Lookout dashboard wiring: its task rows route through the same
    // session paths as the Tasks view (so a completion or edit lands on the
    // owning store), the tab open repaints + widens the sync horizon, and
    // the toolbar's Refresh does the same on demand. The dashboard's own
    // repaint on task changes piggybacks on `refresh_tasks_view`, which
    // every save path funnels through.
    {
        let window = window.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let calendar_state_for_activate = calendar_state.clone();
        let tasks_view_for_activate = tasks_view.clone();
        crate::lookout_view::set_handlers(
            &lookout_view,
            Rc::new(move |task, completed| route_task_toggle(&calendar_state, &tasks_view, task, completed)),
            Rc::new(move |task| open_task_editor_for(&window, &calendar_state_for_activate, &tasks_view_for_activate, &task)),
        );
    }
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let lookout_view = lookout_view.clone();
        lookout_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                // Bypasses `dashboard_refresh`'s visibility gate/debounce -
                // the dashboard becoming visible is exactly the moment it
                // must repaint immediately, not up to 500ms later.
                refresh_lookout_view(&state, &calendar_state, &lookout_view);
                widen_calendar_sync_horizon(&calendar_state);
            }
        });
    }
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let lookout_view = lookout_view.clone();
        lookout_refresh_button.connect_clicked(move |_| {
            // Bypasses the debounce for the same reason: an explicit click
            // asking for a refresh should never wait out the trailing edge.
            refresh_lookout_view(&state, &calendar_state, &lookout_view);
            widen_calendar_sync_horizon(&calendar_state);
        });
    }
    {
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let tasks_view = tasks_view.clone();
        let toast_overlay = toast_overlay.clone();
        connect_google_tasks_button.connect_clicked(move |_| {
            connect_google_tasks(&worker, &calendar_state, &tasks_view, &toast_overlay);
        });
    }
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        calendar_view::connect_event_activated(&calendar_main, move |occ| {
            open_event_editor_for(&window, &state, &worker, &calendar_state, &occ);
        });
    }
    // --- Calendar drag-reschedule: dropping an event chip at a new position
    // in the Day/Week/Work week or Month/Split grids persists the change
    // through the same route the editor's save uses (an etag-guarded
    // `UpdateEvent`, resync on success, error toast on failure). Webcal
    // subscriptions have no write-back path, so their events can't move. A
    // recurring occurrence's drag lands as a per-occurrence override ("This
    // occurrence" scope): the instance moves without re-anchoring the series.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main_for_drag = calendar_main.clone();
        let toast_overlay = toast_overlay.clone();
        calendar_view::connect_event_dragged(&calendar_main, move |occ, new_start, new_end| {
            if calendar_state.borrow().is_read_only_calendar(&occ.calendar_id) {
                toast_overlay.add_toast(adw::Toast::new("Events from calendar subscriptions are read-only."));
                return;
            }
            // Patch the dropped occurrence into the on-screen model and
            // repaint immediately, before the CalDAV round trip even starts -
            // otherwise the chip snaps back to its pre-drag position the
            // instant the drag gesture's own ghost-chip repaint runs, and
            // stays there until the server confirms the move. The repaint
            // itself is deferred one main-loop idle tick: this callback runs
            // while the time grid's drag gesture still holds a borrow on its
            // own `on_drag` slot to invoke it, and `set_time_grid` (which the
            // repaint reaches) re-borrows that same slot - calling it
            // synchronously here panics with "already borrowed". The defer
            // is a same-frame, imperceptible delay, not the round-trip wait
            // this fix removes.
            let mut patched = occ.clone();
            patched.start = new_start;
            patched.end = new_end;
            calendar_state.borrow_mut().pending_calendar_moves.insert((occ.uid.clone(), occ.recurrence_id), patched);
            {
                let calendar_state = calendar_state.clone();
                let calendar_main_for_drag = calendar_main_for_drag.clone();
                glib::idle_add_local_once(move || {
                    refresh_displayed_calendar_view(&calendar_state, &calendar_main_for_drag);
                });
            }
            let mut event = crate::event_editor::calendar_event_from_occurrence(
                &occ,
                new_start.with_timezone(&chrono::Local).naive_local(),
                new_end.with_timezone(&chrono::Local).naive_local(),
            );
            crate::event_editor::apply_edit_scope(&mut event, 0, Some(&occ));
            route_calendar_save(&calendar_state, event.calendar_id.clone(), event);
        });
    }
    // --- Calendar main grid interactions: the first click on an empty time
    // slot in the Day/Week grids (or on a month-grid day cell) selects and
    // highlights it; a second click on the highlighted slot/day opens the
    // new-event editor prefilled for that exact time. Clicking a day cell in
    // the Month/Split grid re-anchors every view to it as it selects - the
    // large grid's counterpart of the sidebar mini-calendar's day buttons.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        calendar_view::connect_slot_activated(&calendar_main, move |start_date, start_minutes, end_date, end_minutes| {
            let Some(start) = start_date.and_hms_opt((start_minutes / 60) as u32, (start_minutes % 60) as u32, 0) else {
                return;
            };
            // The selection covers through the end of its last slot: a
            // 9:00-11:00 drag is a 2h event ending 11:30 (the last 30 minutes
            // fill out the highlighted slot). A single-slot click passes no
            // end, so the editor's usual one-hour default applies.
            let end = if end_date == start_date && end_minutes == start_minutes {
                None
            } else if end_minutes + 30 >= 1440 {
                (end_date + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)
            } else {
                end_date.and_hms_opt(((end_minutes + 30) / 60) as u32, ((end_minutes + 30) % 60) as u32, 0)
            };
            show_new_event_editor(&window, &state, &worker, &calendar_state, &toast_overlay, start, end);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        calendar_view::connect_main_day_selected(&calendar_main, {
            let calendar_main = calendar_main.clone();
            move |date| {
                calendar_view::set_anchor(&calendar_main, date);
                show_anchor(&calendar_state, &calendar_main, &mini_calendar);
            }
        });
    }
    // Clicking a month-grid day cell a second time (it's already highlighted)
    // opens the new-event editor prefilled for 9am of that day.
    {
        let window = window.clone();
        let state = state.clone();
        let worker = worker.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        calendar_view::connect_main_day_activated(&calendar_main, move |date| {
            let Some(start) = date.and_hms_opt(9, 0, 0) else {
                return;
            };
            show_new_event_editor(&window, &state, &worker, &calendar_state, &toast_overlay, start, None);
        });
    }

    // --- Calendar sidebar "Add calendar" -> the subscribe/import/manage
    // dialog (webcal feeds + .ics import). Feeds are read-only, so the dialog
    // is also where they get removed again.
    {
        let window = window.clone();
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_sidebar.calendar_list_box.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let toast_overlay = toast_overlay.clone();
        calendar_sidebar.add_calendar_button.connect_clicked(move |_| {
            show_calendar_dialog(
                &window,
                &state,
                &calendar_state,
                &calendar_main,
                &calendar_list_box,
                &mini_calendar,
                &mail_overview_day,
                &mail_overview_day_list,
                &toast_overlay,
            );
        });
    }

    // --- Webcal feed session: one poller for every configured subscription,
    // started independently of GOA discovery (feeds aren't GOA accounts), so
    // subscriptions load and refresh even with no CalDAV account connected.
    spawn_webcal_session(
        worker.clone(),
        calendar_state.clone(),
        state.borrow().app_config.clone(),
        calendar_main.clone(),
        calendar_sidebar.calendar_list_box.clone(),
        calendar_sidebar.mini_calendar.clone(),
        mail_overview_day.clone(),
        mail_overview_day_list.clone(),
        toast_overlay.clone(),
    );

    // Manually-added ("other") IMAP/SMTP accounts - see `other_accounts.rs`.
    // Unlike GOA there's no D-Bus round trip needed to know the list (it's
    // already persisted in `settings.json`), so these connect synchronously,
    // before `spawn_account_discovery` below kicks off GOA's async discovery.
    // `spawn_account_discovery`'s own "no accounts at all" empty-state check
    // accounts for accounts already connected here.
    {
        let other_accounts = state.borrow().app_config.borrow().other_accounts.clone();
        if !other_accounts.is_empty() {
            for account in other_accounts {
                connect_other_account(
                    worker.clone(),
                    state.clone(),
                    folder_selection.clone(),
                    folder_scroller.clone(),
                    message_list.clone(),
                    message_list_stack.clone(),
                    message_header.clone(),
                    reading_stack.clone(),
                    toast_overlay.clone(),
                    window.clone(),
                    app.clone(),
                    list_header.clone(),
                    account,
                    keyring.clone(),
                    dashboard_refresh.clone(),
                    mark_read_button.clone(),
                    star_button.clone(),
                );
            }
            refresh_config();
        }
    }

    spawn_account_discovery(
        worker.clone(),
        state.clone(),
        root_stack.clone(),
        toast_overlay.clone(),
        window.clone(),
        app.clone(),
        folder_selection,
        folder_scroller.clone(),
        message_list,
        message_list_stack,
        message_header,
        reading_stack,
        current_mail_page,
        mail_view_button,
        list_header,
        refresh_config.clone(),
        current_lookout_page.clone(),
        lookout_view_button.clone(),
        dashboard_refresh.clone(),
        mark_read_button.clone(),
        star_button.clone(),
    );
    spawn_contacts_discovery(
        worker.clone(),
        state.clone(),
        root_stack.clone(),
        toast_overlay.clone(),
        current_contacts_page,
        contacts_view_button,
        refresh_contacts_ui,
    );
    spawn_google_tasks_discovery(worker.clone(), state.clone(), calendar_state.clone(), tasks_view.clone(), toast_overlay.clone());
    spawn_calendar_discovery(
        worker,
        state,
        calendar_state,
        root_stack,
        toast_overlay,
        calendar_main,
        calendar_sidebar.calendar_list_box,
        calendar_sidebar.mini_calendar,
        mail_overview_day,
        mail_overview_day_list,
        current_calendar_page,
        calendar_view_button,
        current_tasks_page,
        tasks_view_button,
        tasks_view,
        reminders_engine,
        refresh_config,
        current_lookout_page,
        lookout_view_button,
    );

    window
}

/// Re-anchors the main calendar panel's already-redrawn views, keeps the
/// sidebar's mini-calendar showing the anchor's month (with event-day
/// markers), records the displayed month, and asks every connected calendar
/// account to resync it.
fn show_anchor(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &CalendarMain, mini_calendar: &calendar_view::MiniCalendar) {
    let day = calendar_view::anchor(calendar_main);
    let month = first_of_month(day);
    let mut st = calendar_state.borrow_mut();
    st.displayed_month = month;
    // The birthdays calendar has no session to ask - recompute the month's
    // occurrences in place so the mini-calendar's event-day markers (and any
    // surface reading `checked_occurrences`) see them immediately.
    if let Some(birthdays) = &mut st.birthdays {
        birthdays.sync_month(month);
    }
    for handle in st.accounts.values() {
        let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
    }
    let webcal_cmd = st.webcal_cmd_tx.clone();
    if let Some(cmd) = webcal_cmd {
        let _ = cmd.send_blocking(SubscriptionCommand::SyncMonth(month));
    }
    drop(st);
    let event_days = calendar_event_days(calendar_state, month);
    calendar_view::set_mini_month(mini_calendar, day, &event_days);
}

/// The local dates within `month` that have at least one occurrence from a
/// currently-checked calendar, unioned across every account that has synced
/// that month - drives the mini-calendar's bold event-day numerals. Every
/// date a multi-day occurrence covers counts, so an event spanning several
/// days marks each of them (matching the main month grid).
fn calendar_event_days(calendar_state: &Rc<RefCell<CalendarUiState>>, month: chrono::NaiveDate) -> HashSet<chrono::NaiveDate> {
    let st = calendar_state.borrow();
    let mut days = HashSet::new();
    let month_start = first_of_month(month);
    let month_end = last_of_month(month);
    for occ in st.checked_occurrences(month) {
        days.extend(calendar_view::covered_local_dates(occ, month_start, month_end));
    }
    days
}

fn current_month_start() -> chrono::NaiveDate {
    first_of_month(chrono::Utc::now().date_naive())
}

fn first_of_month(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    date.with_day(1).unwrap_or(date)
}

/// The last day of `date`'s month.
fn last_of_month(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let first = first_of_month(date);
    let next = if first.month() == 12 {
        chrono::NaiveDate::from_ymd_opt(first.year() + 1, 1, 1).unwrap_or(first)
    } else {
        chrono::NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1).unwrap_or(first)
    };
    next - chrono::Duration::days(1)
}

/// Maps a (nav-rail module, ribbon tab) pair to the `view_toolbar_stack`
/// child to show. Mail is tabbed - Home shows the command toolbar, View the
/// layout toggles; Calendar/Contacts/Config each have a single non-tabbed
/// toolbar of their own, so they ignore the tab. Unknown combos fall back to
/// Mail-Home.
fn ribbon_stack_name(module: &str, tab: &str) -> &'static str {
    match (module, tab) {
        ("mail", "view") => "mail-view",
        ("mail", _) => "mail-home",
        ("calendar", _) => "calendar",
        ("tasks", _) => "tasks",
        ("contacts", _) => "contacts",
        ("lookout", _) => "lookout",
        ("config", _) => "config",
        _ => "mail-home",
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_account_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    current_mail_page: Rc<Cell<&'static str>>,
    mail_view_button: gtk::ToggleButton,
    list_header: ListHeader,
    refresh_config: Rc<dyn Fn()>,
    current_lookout_page: Rc<Cell<&'static str>>,
    lookout_view_button: gtk::ToggleButton,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    star_button: gtk::Button,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_mail_accounts().await?;
            Ok::<_, lookout_goa::Error>((client, accounts))
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        let show_page = |name: &'static str| {
            current_mail_page.set(name);
            if mail_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        // The Lookout tab flips between its status page (no accounts at all)
        // and the live dashboard once *any* account set exists. Mail
        // discovery owns the downgrade: only it knows whether nothing is
        // connected; the calendar discovery only ever upgrades.
        let show_lookout_page = |name: &'static str| {
            current_lookout_page.set(name);
            if lookout_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        match result {
            Ok((client, accounts)) => {
                // Record every discovered account (disabled ones included -
                // Config's account list and a later re-enable need them),
                // then connect only the enabled ones. An account list of
                // nothing-but-disabled accounts therefore falls through to
                // the same empty/fallback handling as GOA reporting none at
                // all - the disabled accounts stay visible in Config.
                let mut enabled_accounts = Vec::new();
                for account in accounts {
                    let id = AccountId(account.account_id.0.clone());
                    let enabled = {
                        let mut st = state.borrow_mut();
                        st.goa_client = Some(client.clone());
                        let entry = st.goa_accounts.entry(id.clone()).or_insert(DiscoveredGoaAccount {
                            display_name: String::new(),
                            email: String::new(),
                            provider_type: None,
                            mail: None,
                            calendar: None,
                            contacts: None,
                        });
                        entry.display_name = account.display_name.clone();
                        entry.email = account.email.clone();
                        entry.provider_type = account.provider_type.clone();
                        entry.mail = Some(account.clone());
                        st.account_enabled(&id)
                    };
                    if enabled {
                        enabled_accounts.push(account);
                    }
                }
                if !enabled_accounts.is_empty() {
                    show_page("mail");
                    show_lookout_page("lookout");
                    // One AccountSession actor per connected account, all
                    // running concurrently on the worker thread. `GoaClient` is
                    // a cheap Arc-backed handle (see its doc comment), so
                    // cloning it per account reuses the one D-Bus connection
                    // rather than opening a redundant one each time.
                    for account in enabled_accounts {
                        connect_account(
                            worker.clone(),
                            state.clone(),
                            folder_selection.clone(),
                            folder_scroller.clone(),
                            message_list.clone(),
                            message_list_stack.clone(),
                            message_header.clone(),
                            reading_stack.clone(),
                            toast_overlay.clone(),
                            window.clone(),
                            app.clone(),
                            client.clone(),
                            list_header.clone(),
                            account,
                            dashboard_refresh.clone(),
                            mark_read_button.clone(),
                            star_button.clone(),
                        );
                    }
                    refresh_config();
                }
                // GOA itself reported no (or zero usable) accounts - but manual
                // "other" accounts are connected synchronously, before this async
                // D-Bus round trip can land, so `state.accounts` may already be
                // non-empty by the time this branch runs. Only show the empty
                // state if nothing at all is connected.
                else if state.borrow().accounts.is_empty() {
                    show_page("empty");
                    show_lookout_page("lookout-empty");
                } else {
                    show_page("mail");
                    show_lookout_page("lookout");
                }
            }
            Err(e) => {
                if state.borrow().accounts.is_empty() {
                    show_page("empty");
                    show_lookout_page("lookout-empty");
                } else {
                    show_page("mail");
                    show_lookout_page("lookout");
                }
                let title = glib::markup_escape_text(&format!("Couldn't reach GNOME Online Accounts: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn connect_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    goa_client: GoaClient,
    list_header: ListHeader,
    account: lookout_goa::GoaMailAccount,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    star_button: gtk::Button,
) {
    let account_id = AccountId(account.account_id.0.clone());
    let display_name = account.display_name.clone();
    let config = AccountConfig {
        account_id: account_id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        imap: EndpointConfig {
            host: account.imap.host.clone(),
            port: account.imap.port.unwrap_or(993),
            use_tls: account.imap.use_tls,
            username: account.imap.username.clone(),
        },
        smtp: EndpointConfig {
            host: account.smtp.host.clone(),
            port: account.smtp.port.unwrap_or(587),
            use_tls: account.smtp.use_tls,
            username: account.smtp.username.clone(),
        },
    };
    let credentials: Rc<dyn lookout_mail::session::CredentialProvider> = if account.is_microsoft_365() {
        // GOA's Microsoft 365 token carries only Microsoft Graph scopes and
        // can't authenticate to Exchange Online IMAP/SMTP (verified live), so
        // Microsoft accounts use Lookout's own OAuth2 flow instead - see
        // `microsoft_oauth.rs`. The first connect opens a browser; afterwards
        // the stored refresh token keeps it silent.
        Rc::new(MicrosoftCredentialProvider::new(account_id.clone()))
    } else {
        Rc::new(GoaCredentialProvider::new(goa_client, account))
    };
    // `run_account_session` requires `Arc<dyn CredentialProvider>` (it may
    // run reconnect attempts on the worker thread's own async tasks), so
    // wrap in a thread-safe handle even though only ever driven from one
    // worker task at a time.
    struct SendWrapper(Rc<dyn lookout_mail::session::CredentialProvider>);
    // SAFETY: the wrapped provider only ever runs on the single worker
    // thread's tokio tasks for this account; it is never touched from the
    // UI thread after being handed off here.
    unsafe impl Send for SendWrapper {}
    unsafe impl Sync for SendWrapper {}
    #[async_trait::async_trait]
    impl lookout_mail::session::CredentialProvider for SendWrapper {
        async fn imap_credential(&self) -> Result<lookout_mail::Credential, String> {
            self.0.imap_credential().await
        }

        async fn smtp_credential(&self) -> Result<lookout_mail::Credential, String> {
            self.0.smtp_credential().await
        }
    }
    let credentials: std::sync::Arc<dyn lookout_mail::session::CredentialProvider> = std::sync::Arc::new(SendWrapper(credentials));

    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    state.borrow_mut().accounts.insert(
        account_id.clone(),
        AccountHandle {
            cmd_tx,
            email: config.email.clone(),
            display_name,
            imap_host: config.imap.host.clone(),
            imap_port: config.imap.port,
            smtp_host: config.smtp.host.clone(),
            smtp_port: config.smtp.port,
            folders: Vec::new(),
            // Opened eagerly so the first composer already has completions.
            // The session opens (and creates) the same file; whichever gets
            // there first wins, and a failure here only costs suggestions.
            address_cache: match lookout_mail::Cache::open(&account_id) {
                Ok(cache) => Some(Arc::new(cache)),
                Err(e) => {
                    tracing::warn!("no address-book cache for {account_id}, recipient autocomplete disabled: {e}");
                    None
                }
            },
        },
    );

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    spawn_account_event_loop(
        evt_rx,
        state,
        account_id,
        folder_selection,
        folder_scroller,
        message_list,
        message_list_stack,
        message_header,
        reading_stack,
        toast_overlay,
        window,
        app,
        list_header,
        dashboard_refresh,
        mark_read_button,
        star_button,
    );
}

/// The identity of a whole-snapshot session event: the one value a later copy
/// of the same event supersedes. An earlier snapshot of a key is dropped by
/// `collapse_last_wins` - replaying both would repaint the same surface twice
/// with the later, strictly-newer data.
#[derive(Clone, PartialEq, Eq, Hash)]
enum SnapshotKey {
    /// `AccountEvent::FoldersUpdated` - the whole folder list.
    Folders,
    /// `AccountEvent::MessagesUpdated` - one mailbox's full message set.
    Mailbox(MailboxId),
    /// `CalendarSessionEvent::CalendarsUpdated` - the whole calendar list.
    Calendars,
    /// `CalendarSessionEvent::OccurrencesUpdated` - one month's occurrences.
    Occurrences(chrono::NaiveDate),
    /// `TasksUpdated` from a calendar account or Google Tasks.
    Tasks,
    /// `GoogleTasksEvent::ListsUpdated`.
    TaskLists,
    /// `SubscriptionSessionEvent::SubscriptionsUpdated` - one month's feeds.
    Subscriptions(chrono::NaiveDate),
}

/// Keeps the events of one drained batch in order, minus the superseded
/// copies: each event for which `supersedable` returns a key survives only in
/// its last occurrence, everything else is untouched in its original position.
/// The startup burst queues the same snapshot several times in a row (cache
/// replay, live sync, previews), and without this the UI would repopulate the
/// same surface once per queued copy.
fn collapse_last_wins<T>(events: Vec<T>, supersedable: impl Fn(&T) -> Option<SnapshotKey>) -> Vec<T> {
    let mut last: HashMap<SnapshotKey, usize> = HashMap::new();
    for (i, event) in events.iter().enumerate() {
        if let Some(key) = supersedable(event) {
            last.insert(key, i);
        }
    }
    let mut collapsed = Vec::with_capacity(events.len());
    for (i, event) in events.into_iter().enumerate() {
        match supersedable(&event) {
            Some(key) if last.get(&key) == Some(&i) => collapsed.push(event),
            Some(_) => {}
            None => collapsed.push(event),
        }
    }
    collapsed
}

fn collapse_account_events(events: Vec<AccountEvent>) -> Vec<AccountEvent> {
    collapse_last_wins(events, |event| match event {
        AccountEvent::FoldersUpdated(_) => Some(SnapshotKey::Folders),
        AccountEvent::MessagesUpdated { mailbox, .. } => Some(SnapshotKey::Mailbox(mailbox.clone())),
        _ => None,
    })
}

fn collapse_calendar_events(events: Vec<CalendarSessionEvent>) -> Vec<CalendarSessionEvent> {
    collapse_last_wins(events, |event| match event {
        CalendarSessionEvent::CalendarsUpdated(_) => Some(SnapshotKey::Calendars),
        CalendarSessionEvent::OccurrencesUpdated { month, .. } => Some(SnapshotKey::Occurrences(*month)),
        CalendarSessionEvent::TasksUpdated(_) => Some(SnapshotKey::Tasks),
        _ => None,
    })
}

fn collapse_google_tasks_events(events: Vec<GoogleTasksEvent>) -> Vec<GoogleTasksEvent> {
    collapse_last_wins(events, |event| match event {
        GoogleTasksEvent::ListsUpdated(_) => Some(SnapshotKey::TaskLists),
        GoogleTasksEvent::TasksUpdated(_) => Some(SnapshotKey::Tasks),
        GoogleTasksEvent::Error(_) => None,
    })
}

fn collapse_subscription_events(events: Vec<SubscriptionSessionEvent>) -> Vec<SubscriptionSessionEvent> {
    collapse_last_wins(events, |event| match event {
        SubscriptionSessionEvent::SubscriptionsUpdated { month, .. } => Some(SnapshotKey::Subscriptions(*month)),
    })
}

/// Drives one connected account's `AccountEvent` stream for the rest of the
/// session, repainting whichever UI surface each event affects. Shared
/// between GOA accounts (`connect_account`) and manually-added accounts
/// (`connect_other_account`) - the event vocabulary and every reaction to it
/// is identical regardless of where the account's credentials come from.
#[allow(clippy::too_many_arguments)]
fn spawn_account_event_loop(
    evt_rx: async_channel::Receiver<AccountEvent>,
    state: Rc<RefCell<UiState>>,
    account_id: AccountId,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    list_header: ListHeader,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    star_button: gtk::Button,
) {
    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing
            // whole-snapshot events (folders, message lists) into the last
            // copy of each - the startup burst queues the same envelope set
            // several times in a row (cache replay, live sync, previews), and
            // the UI must repaint once per batch, not once per queued copy.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_account_events(batch) {
                match event {
                    AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, retryable }) => {
                        // Retryable failures are warnings: the session reconnects
                        // itself with backoff, so they must not pop a toast on
                        // every attempt. Only non-retryable (fatal) errors surface.
                        if !retryable {
                            let title = glib::markup_escape_text(&format!("{}: {message}", account_label(&state, &account_id)));
                            toast_overlay.add_toast(adw::Toast::new(&title));
                        }
                    }
                    AccountEvent::ConnectionStateChanged(_) => {}
                    AccountEvent::FoldersUpdated(folders) => {
                        {
                            let mut st = state.borrow_mut();
                            if let Some(handle) = st.accounts.get_mut(&account_id) {
                                handle.folders = folders;
                            }
                            // Fresh folder list means the account just (re)connected;
                            // any sync requests it left unanswered are dead - clear
                            // them so a later request for the same mailbox isn't
                            // wrongly suppressed by an entry that'll never resolve.
                            st.syncing.retain(|mailbox| mailbox_account_id(mailbox).as_ref() != Some(&account_id));
                        }
                        rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
                        // Folder names and account labels only exist once this
                        // event lands, so a view restored before it (or adopted by
                        // the race below) gets its header filled in here.
                        refresh_list_header(&state, &list_header);
                        // The dashboard's mail stats follow the folder syncs.
                        dashboard_refresh();
                        // A reconnect can clear the open mailbox's `syncing` entry
                        // (its old request is dead) without ever answering it -
                        // don't leave the spinner running for a request that will
                        // never land.
                        refresh_message_loading_state(&state, &message_list, &message_list_stack);
                    }
                    AccountEvent::MessagesUpdated { mailbox, messages } => {
                        // An authoritative sync for this mailbox supersedes
                        // whatever's still sitting in the optimistic-removal
                        // stash - the success path that led here already
                        // dropped those rows from the server too, so there's
                        // nothing left to reconcile.
                        state.borrow_mut().pending_optimistic_removals.remove(&mailbox);
                        state.borrow_mut().pending_optimistic_flag_changes.remove(&mailbox);
                        // The sync this mailbox was asked for (if any) has landed.
                        state.borrow_mut().syncing.remove(&mailbox);
                        refresh_message_loading_state(&state, &message_list, &message_list_stack);
                        // A sync means the envelope cache this dashboard reads
                        // changed - repaint its mail sections. (The calendar
                        // sections repaint via their own event loops.)
                        dashboard_refresh();
                        // While a search is active the results list owns the
                        // pane: a background sync repopulating it would clobber
                        // the results with the folder's full set. Still fold
                        // inbox syncs into the unified snapshot so exiting search
                        // restores fresh data.
                        if state.borrow().search_active {
                            let mut st = state.borrow_mut();
                            let in_unified_set = st
                                .accounts
                                .values()
                                .any(|h| h.folders.iter().any(|m| m.id == mailbox && matches!(m.role, MailboxRole::Inbox)));
                            if in_unified_set {
                                st.unified_snapshots.insert(mailbox, messages);
                            }
                            continue;
                        }
                        // Decide whether this mailbox belongs to the view on
                        // screen, folding its payload into the unified snapshot
                        // when in "All Inboxes" mode. On fresh startup (nothing
                        // selected yet) the first inbox sync is still adopted as
                        // the default single-mailbox view, matching the old
                        // race-first behavior.
                        let (display, single_messages, adopted, unified_slice) = {
                            let mut st = state.borrow_mut();
                            if matches!(st.mail_view, MailView::UnifiedInbox) {
                                // Only accept mailboxes that are actually part
                                // of the unified set (each account's Inbox) - a
                                // stale resync from a mailbox the user last had
                                // open in single-view must not leak into the
                                // merged list.
                                let in_unified_set = st
                                    .accounts
                                    .values()
                                    .any(|h| h.folders.iter().any(|m| m.id == mailbox && matches!(m.role, MailboxRole::Inbox)));
                                // `unified_snapshots` and `message_list`'s own
                                // merged `truth` are independently maintained
                                // (the former is what the 3 full-rebuild call
                                // sites - disconnect, exit_search,
                                // enter_unified_inbox - read from), so both need
                                // this account's new snapshot; the clones here
                                // are O(this account's messages), not O(every
                                // account's).
                                let unified_slice = if in_unified_set {
                                    st.unified_snapshots.insert(mailbox.clone(), messages.clone());
                                    Some(messages)
                                } else {
                                    None
                                };
                                (in_unified_set, None, false, unified_slice)
                            } else {
                                // Nothing selected yet: adopt whichever account's
                                // initial inbox sync lands first as the default
                                // view, rather than leaving the message list empty
                                // until the user clicks a folder. Unless a
                                // remembered view is still pending restore - that
                                // takes priority over the race, so a slow account
                                // can't steal the pane away from the user's last
                                // folder. Whichever connected account happens to
                                // finish its first sync first wins this race - an
                                // acceptable, benign nondeterminism for Phase 1.
                                let adopted = st.current_mailbox.is_none() && !st.restore_pending;
                                if adopted {
                                    st.current_account = Some(account_id.clone());
                                    st.current_mailbox = Some(mailbox.clone());
                                }
                                let is_current = st.current_mailbox.as_ref() == Some(&mailbox);
                                (is_current, is_current.then_some(messages), adopted, None)
                            }
                        };
                        // The adopt-first path picked a default view; name it in
                        // the list header.
                        if adopted {
                            refresh_list_header(&state, &list_header);
                        }
                        if display {
                            let (key, descending) = current_sort(&state);
                            match single_messages {
                                Some(messages) => message_list.repopulate(messages, key, descending),
                                None => {
                                    // `display` in the unified branch is exactly
                                    // `in_unified_set`, which is also what gates
                                    // `unified_slice` being `Some` above - so
                                    // this arm (single-view's `single_messages`
                                    // is always `Some`) only runs when it's set.
                                    let slice = unified_slice.expect("display && single_messages.is_none() implies unified branch set unified_slice");
                                    message_list.repopulate_unified_slice(&mailbox, slice, key, descending);
                                }
                            }
                        }
                    }
                    AccountEvent::NewMessages { mailbox, messages } => {
                        // Desktop notification for genuinely-new unread mail
                        // (`session.rs` already excludes a mailbox's first-ever
                        // sync). Gated by the settings toggle and the mailbox's
                        // role (only Inbox/Custom are worth notifying about - see
                        // `should_notify_role`). Fires even if the window is
                        // already focused on this exact mailbox - the message
                        // just arrived and the list may not have repainted yet,
                        // so the notification is still useful.
                        if state.borrow().settings.get_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED) {
                            let mailbox_info = state.borrow().accounts.get(&account_id).and_then(|h| h.folders.iter().find(|f| f.id == mailbox).cloned());
                            if let Some(mailbox_info) = mailbox_info {
                                if crate::mail_notifications::should_notify_role(mailbox_info.role) {
                                    crate::mail_notifications::show_new_mail_notification(&app, &mailbox_info.name, &mailbox, &messages);
                                }
                            }
                        }
                    }
                    AccountEvent::BodyFetched { mailbox, uid, body } => {
                        let should_render = {
                            let mut st = state.borrow_mut();
                            let is_current = body_request_matches(&mailbox, &uid, st.pending_body_request.as_ref());
                            tracing::debug!(
                                ?mailbox,
                                uid = uid.0,
                                is_current,
                                pending = ?st.pending_body_request,
                                "FetchBody: BodyFetched event received"
                            );
                            // Cache the body regardless of whether the user is
                            // still looking at this message - a fetch that
                            // completed after they moved on is still worth
                            // keeping for when they come back.
                            st.body_cache.insert(mailbox.clone(), uid, body.clone());
                            is_current
                        };
                        if should_render {
                            tracing::debug!(?mailbox, uid = uid.0, "FetchBody: body arrived on UI thread");
                            render_body(&state, &reading_stack, &message_header, mailbox, uid, body);
                        }
                    }
                    AccountEvent::PartFetched { mailbox, uid, part, bytes } => {
                        // An inline `cid:` image request for this part (separate
                        // from the strip's one-at-a-time row actions): finish the
                        // WebKit request with the bytes. Keyed by part number and
                        // dropped on every message change, so a hit is
                        // authoritative - a stale part for the same number can't
                        // linger because the map only ever holds the current
                        // message's requests. Runs first so the strip closures
                        // below can move `part`/`bytes` into their futures.
                        let cid_request = {
                            let mut st = state.borrow_mut();
                            match st.pending_cid.get(&part.part_number) {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(st.pending_cid.remove(&part.part_number).expect("present").request),
                                _ => None,
                            }
                        };
                        if let Some(cid_request) = cid_request {
                            tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, bytes = bytes.len(), "cid: image bytes arrived; finishing WebKit request");
                            let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(&bytes));
                            cid_request.finish(&stream, bytes.len() as i64, Some(&part.content_type));
                        }
                        // An attachment part's bytes arrived. Only act if they
                        // belong to the message currently on the reading pane and
                        // to the outstanding row action - a response that lands
                        // after the user moved on (or for a different part of the
                        // same message) is stale and dropped with its in-flight
                        // bookkeeping.
                        let pending = {
                            let mut st = state.borrow_mut();
                            let on_screen = st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid));
                            if !on_screen {
                                st.pending_attachment = None;
                                None
                            } else {
                                st.pending_attachment.take()
                            }
                        };
                        if let Some(p) = pending {
                            if p.mailbox == mailbox && p.uid == uid && p.part_number == part.part_number {
                                p.button.set_sensitive(true);
                                match p.action {
                                    PendingAttachmentAction::Open => {
                                        let window_for_open = window.clone();
                                        let state_for_open = state.clone();
                                        let toast_for_open = toast_overlay.clone();
                                        glib::spawn_future_local(async move {
                                            open_attachment_temp(&window_for_open, &state_for_open, toast_for_open, &part, &bytes).await;
                                        });
                                    }
                                    PendingAttachmentAction::OpenWith => {
                                        let state_for_open_with = state.clone();
                                        let toast_for_open_with = toast_overlay.clone();
                                        glib::spawn_future_local(async move {
                                            open_attachment_with(&state_for_open_with, toast_for_open_with, &part, &bytes).await;
                                        });
                                    }
                                    PendingAttachmentAction::Save => {
                                        let window_for_save = window.clone();
                                        let toast_for_save = toast_overlay.clone();
                                        glib::spawn_future_local(async move {
                                            save_attachment_to_disk(&window_for_save, toast_for_save, &part, &bytes).await;
                                        });
                                    }
                                }
                            } else {
                                // Mismatched request (shouldn't happen with the
                                // one-at-a-time guard, but be safe).
                                state.borrow_mut().pending_attachment = Some(p);
                            }
                        }
                    }
                    AccountEvent::PartFetchFailed {
                        mailbox,
                        uid,
                        part_number,
                        message,
                    } => {
                        // The session couldn't produce this attachment's bytes.
                        // Restore the row's button if this is still the outstanding
                        // request and tell the user what went wrong - never leave
                        // it stuck.
                        let pending = {
                            let mut st = state.borrow_mut();
                            let on_screen = st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid));
                            if !on_screen {
                                st.pending_attachment = None;
                                None
                            } else {
                                st.pending_attachment.take()
                            }
                        };
                        if let Some(p) = pending {
                            if p.mailbox == mailbox && p.uid == uid && p.part_number == part_number {
                                p.button.set_sensitive(true);
                                toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                            } else {
                                state.borrow_mut().pending_attachment = Some(p);
                            }
                        }
                        // The same failure for an inline `cid:` image request:
                        // finish the WebKit request with an error so the browser
                        // draws a broken image rather than waiting forever.
                        let cid_request = {
                            let mut st = state.borrow_mut();
                            match st.pending_cid.get(&part_number) {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(st.pending_cid.remove(&part_number).expect("present").request),
                                _ => None,
                            }
                        };
                        if let Some(cid_request) = cid_request {
                            tracing::warn!(?mailbox, uid = uid.0, part = %part_number, "cid: inline image fetch failed: {message}");
                            finish_cid_request_error(&cid_request, "the inline image could not be fetched");
                        }
                    }
                    AccountEvent::RawMessageFetched { mailbox, uid, bytes } => {
                        // The .eml export's whole raw message arrived. Only act if
                        // it belongs to the outstanding export request; a response
                        // that lands after the user selected a different message
                        // (or triggered a newer export) is stale and dropped with
                        // its bookkeeping.
                        let pending = {
                            let mut st = state.borrow_mut();
                            match st.pending_raw_message.take() {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(p),
                                other => {
                                    st.pending_raw_message = other;
                                    None
                                }
                            }
                        };
                        if let Some(pending) = pending {
                            let window_for_save = window.clone();
                            let toast_for_save = toast_overlay.clone();
                            glib::spawn_future_local(async move {
                                save_raw_message_to_disk(&window_for_save, toast_for_save, &pending.initial_name, &bytes).await;
                            });
                        }
                    }
                    AccountEvent::RawMessageFetchFailed { mailbox, uid, message } => {
                        // The session couldn't produce the raw message. Clear the
                        // pending export (so a later click can retry) and tell the
                        // user what went wrong - never leave it stuck.
                        let pending = {
                            let mut st = state.borrow_mut();
                            match st.pending_raw_message.take() {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(p),
                                other => {
                                    st.pending_raw_message = other;
                                    None
                                }
                            }
                        };
                        if pending.is_some() {
                            toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                        }
                    }
                    AccountEvent::SendCompleted => {
                        if let Some(toast) = state.borrow_mut().sending_toasts.get_mut(&account_id).and_then(VecDeque::pop_front) {
                            toast.dismiss();
                        }
                        toast_overlay.add_toast(adw::Toast::new("Message sent"));
                    }
                    AccountEvent::SendFailed(message) => {
                        if let Some(toast) = state.borrow_mut().sending_toasts.get_mut(&account_id).and_then(VecDeque::pop_front) {
                            toast.dismiss();
                        }
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                        // The toast above is enough while the window is focused;
                        // a desktop notification is for when the user isn't
                        // looking, so a failed send doesn't go unnoticed.
                        if !window.is_active() && state.borrow().settings.get_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED) {
                            crate::mail_notifications::show_send_failed_notification(&app, &account_id, &message);
                        }
                    }
                    AccountEvent::DraftSaved { message_id } => {
                        // Relay the confirmation to whichever composer is open;
                        // it decides whether the id is its own.
                        if let Some(tx) = state.borrow().draft_saved_tx.clone() {
                            let _ = tx.try_send(message_id);
                        }
                    }
                    AccountEvent::MessageMoved { role } => {
                        let label = match role {
                            MailboxRole::Trash => "Deleted",
                            MailboxRole::Archive => "Archived",
                            MailboxRole::Junk => "Reported as junk",
                            _ => "Moved",
                        };
                        toast_overlay.add_toast(adw::Toast::new(label));
                    }
                    AccountEvent::MoveFailed { mailbox, uids, role, message } => {
                        // The move actually failed server-side, so put back
                        // exactly the rows `optimistic_remove_messages` hid for
                        // this attempt - the row must not stay gone for a move
                        // that never happened.
                        restore_optimistic_removals(&state, &message_list, &mailbox, &uids);
                        let verb = match role {
                            MailboxRole::Trash => "delete",
                            MailboxRole::Archive => "archive",
                            MailboxRole::Junk => "report as junk",
                            _ => "move",
                        };
                        let title = glib::markup_escape_text(&format!("Couldn't {verb}: {message}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    AccountEvent::StoreFlagsFailed { mailbox, uids, message } => {
                        // The flag change actually failed server-side, so put
                        // back exactly the summaries `optimistic_toggle_read`
                        // patched for this attempt - a no-op if this failure
                        // wasn't from a mark-read/unread toggle to begin with
                        // (e.g. a Star/Unstar failure, which isn't optimistic).
                        restore_optimistic_flag_changes(&state, &message_list, &mailbox, &uids);
                        refresh_mark_read_button(&mark_read_button, &message_list);
                        // Reverts the click handler's optimistic icon flip back
                        // to whatever the (unpatched, so still correct) selected
                        // summaries actually say.
                        refresh_star_button(&star_button, &message_list);
                        let title = glib::markup_escape_text(&format!("Couldn't update message flags: {message}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    AccountEvent::MailboxExpunged { role } => {
                        let label = match role {
                            MailboxRole::Trash => "Trash emptied",
                            MailboxRole::Junk => "Junk emptied",
                            _ => "Folder emptied",
                        };
                        toast_overlay.add_toast(adw::Toast::new(label));
                    }
                    AccountEvent::MessageSnoozed => {
                        toast_overlay.add_toast(adw::Toast::new("Snoozed until tomorrow 9:00 AM"));
                    }
                    AccountEvent::SearchResults { mailbox, query, messages } => {
                        // An answer to the live IMAP pass. `mailbox` matches a
                        // `search_pending` entry (the session always answers, even
                        // with an empty match set); a result whose folder is no
                        // longer pending - or whose query isn't the one on screen,
                        // for the same-folder re-search race where an old answer
                        // lands after the folder is pending again - belongs to a
                        // stale search and is dropped.
                        let account_id = mailbox_account_id(&mailbox);
                        let pending_key = account_id.as_ref().map(|id| (id.clone(), mailbox.clone()));
                        let wanted = pending_key.as_ref().is_some_and(|key| {
                            let mut st = state.borrow_mut();
                            st.search_pending.remove(key)
                        });
                        if !wanted || state.borrow().search_query != query {
                            continue;
                        }
                        // Merge the answer into the accumulated results - a
                        // search can also have surfaced this message in the cache
                        // pass, so dedupe by `(mailbox, uid)`.
                        let mut seen: HashSet<(MailboxId, Uid)> = state.borrow().search_results.iter().map(|m| (m.mailbox.clone(), m.uid)).collect();
                        let mut st = state.borrow_mut();
                        for m in messages {
                            if seen.insert((m.mailbox.clone(), m.uid)) {
                                st.search_results.push(m);
                            }
                        }
                        drop(st);
                        repopulate_search_results(&state, &message_list);
                    }
                    AccountEvent::Error(message) => {
                        let title = glib::markup_escape_text(&format!("{}: {message}", account_label(&state, &account_id)));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                }
            }
        }
    });
}

/// Connects one manually-added ("other") IMAP/SMTP account - the
/// non-GOA counterpart to `connect_account`. Builds the session's
/// `AccountConfig` directly from the persisted `OtherAccount` fields instead
/// of a `GoaMailAccount`, and its credentials come from the GNOME keyring
/// (`OtherCredentialProvider`) rather than GOA/Microsoft OAuth - everything
/// past that point (the `AccountHandle`, the session actor, the event loop)
/// is identical, so it's shared via `spawn_account_event_loop`.
#[allow(clippy::too_many_arguments)]
fn connect_other_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    list_header: ListHeader,
    account: crate::app_config::OtherAccount,
    keyring: std::sync::Arc<dyn crate::other_accounts::KeyringStore>,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    star_button: gtk::Button,
) {
    let account_id = AccountId(account.id.clone());
    let display_name = account.display_name.clone();
    let config = AccountConfig {
        account_id: account_id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        imap: EndpointConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            use_tls: account.imap_use_tls,
            username: account.imap_username.clone(),
        },
        smtp: EndpointConfig {
            host: account.smtp_host.clone(),
            port: account.smtp_port,
            use_tls: account.smtp_use_tls,
            username: account.smtp_username.clone(),
        },
    };
    // Unlike the GOA/Microsoft providers, `OtherCredentialProvider` holds no
    // `Rc`s (only an `Arc<dyn KeyringStore>`), so it's already `Send + Sync`
    // and needs no `SendWrapper` shim to satisfy `run_account_session`'s
    // `Arc<dyn CredentialProvider>` bound.
    let credentials: std::sync::Arc<dyn lookout_mail::session::CredentialProvider> =
        std::sync::Arc::new(crate::other_accounts::OtherCredentialProvider::new(account_id.clone(), keyring));

    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    state.borrow_mut().accounts.insert(
        account_id.clone(),
        AccountHandle {
            cmd_tx,
            email: config.email.clone(),
            display_name,
            imap_host: config.imap.host.clone(),
            imap_port: config.imap.port,
            smtp_host: config.smtp.host.clone(),
            smtp_port: config.smtp.port,
            folders: Vec::new(),
            // Opened eagerly so the first composer already has completions.
            // The session opens (and creates) the same file; whichever gets
            // there first wins, and a failure here only costs suggestions.
            address_cache: match lookout_mail::Cache::open(&account_id) {
                Ok(cache) => Some(Arc::new(cache)),
                Err(e) => {
                    tracing::warn!("no address-book cache for {account_id}, recipient autocomplete disabled: {e}");
                    None
                }
            },
        },
    );

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    spawn_account_event_loop(
        evt_rx,
        state,
        account_id,
        folder_selection,
        folder_scroller,
        message_list,
        message_list_stack,
        message_header,
        reading_stack,
        toast_overlay,
        window,
        app,
        list_header,
        dashboard_refresh,
        mark_read_button,
        star_button,
    );
}

/// Tears a GOA account down after the user disables it in Config → Accounts:
/// every running service (mail, calendar, Google Tasks, contacts) stops by
/// dropping its handle - which closes the session's command channel, the
/// same clean-shutdown convention the other-account Remove path relies on -
/// per-account state is pruned, and every view is repainted so nothing from
/// the account remains visible. The on-disk caches are deliberately kept: a
/// re-enable reconnects to them immediately (see `connect_goa_account`).
#[allow(clippy::too_many_arguments)]
fn teardown_goa_account(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    account_id: &AccountId,
    email: Option<&str>,
    folder_selection: &gtk::SingleSelection,
    folder_scroller: &gtk::ScrolledWindow,
    message_list: &MessageListModel,
    message_list_stack: &gtk::Stack,
    list_header: &ListHeader,
    calendar_main: &Rc<CalendarMain>,
    calendar_list_box: &gtk::Box,
    reminders_engine: &Rc<RefCell<crate::reminders::ReminderEngine>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    refresh_contacts_ui: &Rc<dyn Fn(Option<i32>)>,
    dashboard_refresh: &Rc<dyn Fn()>,
) {
    // -- mail -- Prune every per-mailbox piece of state the account feeds so
    // no stale data survives, and note whether the open mailbox belonged to
    // it (the view must navigate away or the messages stay on screen).
    let open_mailbox_was_this_account = {
        let mut st = state.borrow_mut();
        st.accounts.remove(account_id);
        let belongs = |mailbox: &MailboxId| mailbox_account_id(mailbox).as_ref() == Some(account_id);
        st.unified_snapshots.retain(|mailbox, _| !belongs(mailbox));
        st.syncing.retain(|mailbox| !belongs(mailbox));
        st.pending_optimistic_removals.retain(|mailbox, _| !belongs(mailbox));
        st.pending_optimistic_flag_changes.retain(|mailbox, _| !belongs(mailbox));
        st.search_results.retain(|summary| !belongs(&summary.mailbox));
        st.search_pending.retain(|(_, mailbox)| !belongs(mailbox));
        st.current_mailbox.as_ref().is_some_and(belongs)
    };
    if open_mailbox_was_this_account {
        enter_unified_inbox(state, message_list, message_list_stack);
    }
    // A unified-view list drawn from the disabled account's inbox snapshots
    // must repopulate even when no mailbox was open.
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
        refresh_message_loading_state(state, message_list, message_list_stack);
    }
    rebuild_folder_tree(state, folder_selection, folder_scroller);
    refresh_list_header(state, list_header);
    dashboard_refresh();

    // -- calendar, Google Tasks, reminders --
    {
        let mut st = calendar_state.borrow_mut();
        st.accounts.remove(account_id);
        // Calendar ids embed their account's id as a prefix; drop every
        // checked entry belonging to the disabled account.
        let prefix = format!("{}:", account_id.0);
        st.checked_calendar_ids.retain(|id| !id.0.starts_with(&prefix));
        if let Some(email) = email {
            st.google_tasks.remove(email);
            st.google_account_emails.retain(|e| e != email);
        }
    }
    reminders_engine.borrow_mut().remove_account(account_id);
    refresh_calendar_checklist(calendar_state, calendar_list_box, calendar_main);
    refresh_displayed_calendar_view(calendar_state, calendar_main);
    refresh_tasks_view(calendar_state, tasks_view);

    // -- contacts --
    {
        let mut st = state.borrow_mut();
        st.contacts_by_account.remove(account_id);
        st.deleted_contacts.remove(account_id);
        st.contact_cmd_tx.remove(account_id);
    }
    refresh_contacts_ui(None);
}

/// Reconnects every service a GOA account advertises after the user
/// re-enables it in Config → Accounts, from the account's stored discovery
/// structs (`UiState::goa_accounts`) - no discovery re-run needed. Also
/// flips the relevant pages back from their empty states, since a re-enabled
/// account is live again the moment its sessions spawn.
#[allow(clippy::too_many_arguments)]
fn connect_goa_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    account_id: &AccountId,
    account: &DiscoveredGoaAccount,
    root_stack: gtk::Stack,
    current_mail_page: Rc<Cell<&'static str>>,
    mail_view_button: gtk::ToggleButton,
    current_calendar_page: Rc<Cell<&'static str>>,
    calendar_view_button: gtk::ToggleButton,
    current_tasks_page: Rc<Cell<&'static str>>,
    tasks_view_button: gtk::ToggleButton,
    current_lookout_page: Rc<Cell<&'static str>>,
    lookout_view_button: gtk::ToggleButton,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    list_header: ListHeader,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    star_button: gtk::Button,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    reminders_engine: Rc<RefCell<crate::reminders::ReminderEngine>>,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    refresh_contacts_ui: Rc<dyn Fn(Option<i32>)>,
) {
    let Some(goa_client) = state.borrow().goa_client.clone() else {
        tracing::warn!("no GOA client available to reconnect {account_id}");
        return;
    };
    let account_id = account_id.clone();
    if let Some(mail) = &account.mail {
        connect_account(
            worker.clone(),
            state.clone(),
            folder_selection.clone(),
            folder_scroller.clone(),
            message_list.clone(),
            message_list_stack.clone(),
            message_header.clone(),
            reading_stack.clone(),
            toast_overlay.clone(),
            window.clone(),
            app.clone(),
            goa_client.clone(),
            list_header.clone(),
            mail.clone(),
            dashboard_refresh.clone(),
            mark_read_button.clone(),
            star_button.clone(),
        );
        current_mail_page.set("mail");
        if mail_view_button.is_active() {
            root_stack.set_visible_child_name("mail");
        }
    }
    if let Some(calendar) = &account.calendar {
        connect_calendar_account(
            worker.clone(),
            calendar_state.clone(),
            calendar_main.clone(),
            calendar_list_box.clone(),
            mini_calendar.clone(),
            mail_overview_day.clone(),
            mail_overview_day_list.clone(),
            reminders_engine.clone(),
            toast_overlay.clone(),
            goa_client.clone(),
            calendar.clone(),
            tasks_view.clone(),
        );
        current_calendar_page.set("calendar");
        if calendar_view_button.is_active() {
            root_stack.set_visible_child_name("calendar");
        }
        current_tasks_page.set("tasks");
        if tasks_view_button.is_active() {
            root_stack.set_visible_child_name("tasks");
        }
    }
    if let Some(contacts) = &account.contacts {
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        state.borrow_mut().contact_cmd_tx.insert(account_id.clone(), cmd_tx);
        sync_contacts_account(
            worker.clone(),
            state.clone(),
            toast_overlay.clone(),
            goa_client.clone(),
            contacts.clone(),
            cmd_rx,
            refresh_contacts_ui.clone(),
        );
    }
    // Google Tasks needs a stored OAuth token to connect silently; without
    // one it just reappears as a "Connect Google Tasks" button target.
    if account.provider_type.as_deref() == Some("google") {
        if !calendar_state.borrow().google_account_emails.contains(&account.email) {
            calendar_state.borrow_mut().google_account_emails.push(account.email.clone());
        }
        if google_tasks::has_stored_token(&account.email) && !calendar_state.borrow().google_tasks.contains_key(&account.email) {
            connect_google_tasks_account(worker.clone(), calendar_state.clone(), tasks_view.clone(), toast_overlay.clone(), account.email.clone());
        }
    }
    // The Lookout dashboard goes live as soon as any account set exists.
    if !state.borrow().accounts.is_empty() || !calendar_state.borrow().accounts.is_empty() {
        current_lookout_page.set("lookout");
        if lookout_view_button.is_active() {
            root_stack.set_visible_child_name("lookout");
        }
    }
}

/// Mirrors `spawn_account_discovery` 1:1 for Calendar - a fully independent
/// GOA account set, discovered and connected the same worker-spawn +
/// `glib::spawn_future_local` way.
#[allow(clippy::too_many_arguments)]
fn spawn_calendar_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    current_calendar_page: Rc<Cell<&'static str>>,
    calendar_view_button: gtk::ToggleButton,
    current_tasks_page: Rc<Cell<&'static str>>,
    tasks_view_button: gtk::ToggleButton,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    reminders_engine: Rc<RefCell<crate::reminders::ReminderEngine>>,
    refresh_config: Rc<dyn Fn()>,
    current_lookout_page: Rc<Cell<&'static str>>,
    lookout_view_button: gtk::ToggleButton,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_calendar_accounts().await?;
            Ok::<_, lookout_goa::Error>((client, accounts))
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        let show_page = |name: &'static str| {
            current_calendar_page.set(name);
            if calendar_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        let show_tasks_page = |name: &'static str| {
            current_tasks_page.set(name);
            if tasks_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        // The Lookout tab goes live as soon as *any* account set exists.
        // Deliberately upgrade-only: the mail discovery owns the downgrade
        // to "lookout-empty", so finding no calendar accounts here must not
        // hide the dashboard when mail accounts exist.
        let show_lookout_page = |name: &'static str| {
            current_lookout_page.set(name);
            if lookout_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        match result {
            Ok((client, accounts)) => {
                // Record every discovered calendar account into `state`'s GOA
                // union (disabled ones included - see the mail discovery's
                // note), then connect only the enabled ones. A nothing-but-
                // disabled result falls through to the same empty handling
                // as GOA reporting no calendar accounts at all.
                let mut enabled_accounts = Vec::new();
                for account in accounts {
                    let id = account.account_id.clone();
                    let enabled = {
                        let mut st = state.borrow_mut();
                        st.goa_client = Some(client.clone());
                        let entry = st.goa_accounts.entry(id.clone()).or_insert(DiscoveredGoaAccount {
                            display_name: String::new(),
                            email: String::new(),
                            provider_type: None,
                            mail: None,
                            calendar: None,
                            contacts: None,
                        });
                        entry.display_name = account.display_name.clone();
                        entry.email = account.display_name.clone();
                        entry.provider_type = account.provider_type.clone();
                        entry.calendar = Some(account.clone());
                        st.account_enabled(&id)
                    };
                    if enabled {
                        enabled_accounts.push(account);
                    }
                }
                if !enabled_accounts.is_empty() {
                    show_page("calendar");
                    show_tasks_page("tasks");
                    show_lookout_page("lookout");
                    for account in enabled_accounts {
                        connect_calendar_account(
                            worker.clone(),
                            calendar_state.clone(),
                            calendar_main.clone(),
                            calendar_list_box.clone(),
                            mini_calendar.clone(),
                            mail_overview_day.clone(),
                            mail_overview_day_list.clone(),
                            reminders_engine.clone(),
                            toast_overlay.clone(),
                            client.clone(),
                            account,
                            tasks_view.clone(),
                        );
                    }
                    refresh_config();
                } else {
                    show_page("calendar-empty");
                    show_tasks_page("tasks-empty");
                }
            }
            Err(e) => {
                show_page("calendar-empty");
                show_tasks_page("tasks-empty");
                let title = glib::markup_escape_text(&format!("Couldn't reach GNOME Online Accounts: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
            }
        }
    });
}

fn ensure_checked_calendars(checked: &mut HashSet<CalendarId>, calendars: &[CalendarInfo]) {
    for calendar in calendars {
        checked.insert(calendar.id.clone());
    }
}

/// Starts the Google Tasks integration: lists every Google GOA calendar
/// account (the provider that can't store CalDAV tasks), remembers their
/// emails for the "Connect Google Tasks" button, and auto-connects those
/// with a stored refresh token - the non-interactive path, since a stored
/// token means the user authorized once already.
fn spawn_google_tasks_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    toast_overlay: adw::ToastOverlay,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_calendar_accounts().await?;
            Ok::<_, lookout_goa::Error>(accounts)
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        match result {
            Ok(accounts) => {
                // Disabled accounts are excluded here too - a disabled Google
                // account must neither auto-connect nor appear as a "Connect
                // Google Tasks" target.
                let emails: Vec<String> = accounts
                    .iter()
                    .filter(|a| a.provider_type.as_deref() == Some("google") && state.borrow().account_enabled(&a.account_id))
                    .map(|a| a.display_name.clone())
                    .collect();
                {
                    let mut st = calendar_state.borrow_mut();
                    st.google_account_emails = emails.clone();
                }
                for email in emails {
                    if google_tasks::has_stored_token(&email) && !calendar_state.borrow().google_tasks.contains_key(&email) {
                        connect_google_tasks_account(worker.clone(), calendar_state.clone(), tasks_view.clone(), toast_overlay.clone(), email);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("couldn't discover Google Tasks accounts: {e}");
            }
        }
    });
}

/// The interactive "Connect Google Tasks" action: runs the OAuth
/// authorization-code flow (opening a browser) for the account's first
/// Google GOA account, then spawns the sync session. Rejects politely when
/// there's no Google account or it's already connected.
fn connect_google_tasks(worker: &Rc<Worker>, calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>, toast_overlay: &adw::ToastOverlay) {
    let email = {
        let st = calendar_state.borrow();
        let Some(email) = st.google_account_emails.first().cloned() else {
            toast_overlay.add_toast(adw::Toast::new("Add a Google account in GNOME Online Accounts first."));
            return;
        };
        if st.google_tasks.contains_key(&email) {
            toast_overlay.add_toast(adw::Toast::new("Google Tasks is already connected."));
            return;
        }
        email
    };

    let worker = worker.clone();
    let calendar_state = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    let toast_overlay = toast_overlay.clone();
    let (tx, rx) = async_channel::bounded(1);
    let email_for_auth = email.clone();
    worker.spawn(async move {
        let oauth = google_tasks::GoogleTasksOAuth::new(&email_for_auth);
        let _ = tx.send(oauth.access_token().await).await;
    });
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(_token)) => {
                let title = glib::markup_escape_text(&format!("Google Tasks connected for {email}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
                connect_google_tasks_account(worker, calendar_state, tasks_view, toast_overlay, email);
            }
            Ok(Err(e)) => {
                let title = glib::markup_escape_text(&format!("Couldn't connect Google Tasks: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
            }
            Err(_) => {}
        }
    });
}

/// Spawns one Google account's Tasks session and routes its events into the
/// Tasks view: lists become picker entries (with calendar colours), task
/// snapshots merge into the view, and errors toast.
fn connect_google_tasks_account(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    toast_overlay: adw::ToastOverlay,
    email: String,
) {
    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    calendar_state.borrow_mut().google_tasks.insert(
        email.clone(),
        GoogleTasksHandle {
            cmd_tx,
            email: email.clone(),
            task_lists: Vec::new(),
            last_tasks: Vec::new(),
            error: None,
        },
    );
    worker.spawn(google_tasks::run_google_tasks_session(email.clone(), cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing
            // whole-snapshot events (lists, tasks) into the last copy of each
            // - the cache fast-paint and the first live poll queue the same
            // snapshots back to back.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_google_tasks_events(batch) {
                match event {
                    GoogleTasksEvent::ListsUpdated(lists) => {
                        {
                            let mut st = calendar_state.borrow_mut();
                            if let Some(handle) = st.google_tasks.get_mut(&email) {
                                handle.task_lists = lists.clone();
                                handle.error = None;
                            }
                            let infos: Vec<CalendarInfo> = lists
                                .iter()
                                .map(|list| CalendarInfo {
                                    id: google_tasks::google_task_calendar_id(&list.id),
                                    account_id: AccountId(format!("googletasks:{email}")),
                                    display_name: list.title.clone(),
                                    color: None,
                                    href: String::new(),
                                    supports_tasks: true,
                                })
                                .collect();
                            calendar_colors::assign_missing(&mut st.calendar_colors, &infos);
                        }
                        refresh_tasks_view(&calendar_state, &tasks_view);
                    }
                    GoogleTasksEvent::TasksUpdated(tasks) => {
                        if let Some(handle) = calendar_state.borrow_mut().google_tasks.get_mut(&email) {
                            handle.last_tasks = tasks;
                            handle.error = None;
                        }
                        refresh_tasks_view(&calendar_state, &tasks_view);
                    }
                    GoogleTasksEvent::Error(message) => {
                        if let Some(handle) = calendar_state.borrow_mut().google_tasks.get_mut(&email) {
                            handle.error = Some(message.clone());
                        }
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                    }
                }
            }
        }
    });
}

impl CalendarUiState {
    /// Every checked calendar's latest occurrences for `month`, unioned
    /// across accounts, webcal feeds, and the birthdays calendar - the one
    /// merge every calendar-facing surface (main view, print, mini-calendar
    /// event days, agenda) uses. Callers must have synced the birthdays
    /// handle for `month` first (the contacts hook and `show_anchor` do).
    fn checked_occurrences(&self, month: chrono::NaiveDate) -> Vec<&EventOccurrence> {
        self.accounts
            .values()
            .filter(|h| h.last_synced_month == Some(month))
            .flat_map(|h| h.last_occurrences.iter())
            .chain(
                self.webcal_handles
                    .values()
                    .filter(|h| h.last_synced_month == Some(month))
                    .flat_map(|h| h.last_occurrences.iter()),
            )
            .chain(
                self.birthdays
                    .as_ref()
                    .filter(|h| h.last_synced_month == Some(month))
                    .into_iter()
                    .flat_map(|h| h.last_occurrences.iter()),
            )
            .filter(|occ| self.checked_calendar_ids.contains(&occ.calendar_id))
            .collect()
    }

    /// Like [`Self::checked_occurrences`], but across every synced month -
    /// the surfaces that filter by exact day (the mail-overview day list,
    /// the event editor's preview) rather than by the displayed month.
    fn checked_occurrences_all_months(&self) -> Vec<&EventOccurrence> {
        self.accounts
            .values()
            .flat_map(|h| h.last_occurrences.iter())
            .chain(self.webcal_handles.values().flat_map(|h| h.last_occurrences.iter()))
            .chain(self.birthdays.as_ref().into_iter().flat_map(|h| h.last_occurrences.iter()))
            .filter(|occ| self.checked_calendar_ids.contains(&occ.calendar_id))
            .collect()
    }

    /// Whether `calendar_id` is a read-only synthetic calendar (a webcal
    /// feed, or the birthdays calendar) - shared by the editor's read-only
    /// mode, the drag guard, and the delete route, so one id-based check
    /// covers every "no write-back path" source.
    fn is_read_only_calendar(&self, calendar_id: &CalendarId) -> bool {
        self.read_only_note(calendar_id).is_some()
    }

    /// The read-only explanation the event editor shows under the form for a
    /// synthetic calendar, or `None` when the calendar is writable.
    fn read_only_note(&self, calendar_id: &CalendarId) -> Option<&'static str> {
        if self.webcal_handles.values().any(|h| &h.calendar_id == calendar_id) {
            Some("This calendar is a read-only subscription - changes can't be saved back to the feed.")
        } else if self.birthdays.as_ref().is_some_and(|h| &h.calendar_id == calendar_id) {
            Some("This birthday is synthesized from your contacts - it can't be edited or deleted.")
        } else {
            None
        }
    }
}

/// Copies the current contacts snapshots into the birthdays handle
/// (creating it on the first contact, dropping it when the last contact
/// goes away) and recomputes the displayed month plus the dashboard
/// horizon, then feeds the reminder engine the freshly-computed
/// occurrences - the `OccurrencesUpdated`-ingest equivalent for a calendar
/// that has no session of its own. Returns whether any contacts exist (the
/// caller only runs the render funnel when they do).
fn refresh_birthdays_from_contacts(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    reminders_engine: &Rc<RefCell<crate::reminders::ReminderEngine>>,
) -> bool {
    let contacts: Vec<(AccountId, Vec<lookout_dav::ContactRecord>)> = {
        let st = state.borrow();
        st.contacts_by_account
            .iter()
            .map(|(account_id, snapshot)| (account_id.clone(), snapshot.contacts.clone()))
            .collect()
    };
    if contacts.is_empty() {
        // No contacts anywhere - hide the calendar (the checklist row
        // disappears with it) rather than show an empty "Birthdays" row.
        calendar_state.borrow_mut().birthdays = None;
        return false;
    }
    let mut st = calendar_state.borrow_mut();
    let month = st.displayed_month;
    let handle = st.birthdays.get_or_insert_with(|| BirthdaysHandle {
        calendar_id: birthdays_calendar_id(),
        display_name: "Birthdays".to_string(),
        contacts: Vec::new(),
        last_occurrences: Vec::new(),
        last_synced_month: None,
        occurrences_by_month: HashMap::new(),
    });
    handle.set_contacts(contacts);
    handle.sync_month(month);
    handle.sync_dashboard_window();
    drop(st);
    // Ingest both the displayed month and the dashboard horizon, so an alert
    // for an upcoming month's birthday is already in the engine before the
    // calendar view ever navigates there (the same union the CalDAV
    // sessions' `OccurrencesUpdated` ingests build up month by month).
    let occurrences: Vec<EventOccurrence> = {
        let st = calendar_state.borrow();
        let handle = st.birthdays.as_ref().expect("created just above");
        handle
            .last_occurrences
            .iter()
            .cloned()
            .chain(handle.occurrences_by_month.values().flatten().cloned())
            .collect()
    };
    reminders_engine.borrow_mut().ingest(&AccountId("birthdays".to_string()), &occurrences);
    true
}

/// Recomputes which calendars actually exist across every connected account,
/// defaults any newly-seen id to checked (shown), and re-renders the
/// sidebar's "My calendars" checklist against that - the checklist's own
/// `on_toggle` closure flips membership in `checked_calendar_ids` and calls
/// `refresh_displayed_calendar_view` to redraw the grid accordingly.
fn refresh_calendar_checklist(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_list_box: &gtk::Box, calendar_main: &Rc<CalendarMain>) {
    let mut groups: Vec<calendar_view::CalendarAccountGroup> = calendar_state
        .borrow()
        .accounts
        .values()
        .map(|h| calendar_view::CalendarAccountGroup {
            display_name: h.display_name.clone(),
            calendars: h.calendars.clone(),
            status: calendar_view::calendar_account_status_text(&h.connection_state, !h.calendars.is_empty()),
        })
        .collect();
    // Subscriptions render as one synthetic "Webcal subscriptions" group so
    // they inherit the checklist's toggle/color machinery unchanged.
    {
        let st = calendar_state.borrow();
        if !st.webcal_subscriptions.is_empty() {
            groups.push(calendar_view::CalendarAccountGroup {
                display_name: "Webcal subscriptions".to_string(),
                calendars: st
                    .webcal_subscriptions
                    .iter()
                    .map(|sub| CalendarInfo {
                        id: webcal_calendar_id(&sub.id),
                        account_id: AccountId(format!("webcal:{}", sub.id)),
                        display_name: sub.display_name.clone(),
                        color: None,
                        href: sub.url.clone(),
                        // Feeds are events-only - never a task target.
                        supports_tasks: false,
                    })
                    .collect(),
                status: None,
            });
        }
    }
    // The synthesized birthdays calendar renders as one row in its own
    // group, present only while any contacts exist (the handle is `None`
    // otherwise) - same inheritance of the toggle/color machinery.
    {
        let st = calendar_state.borrow();
        if let Some(birthdays) = &st.birthdays {
            groups.push(calendar_view::CalendarAccountGroup {
                display_name: "Birthdays".to_string(),
                calendars: vec![CalendarInfo {
                    id: birthdays.calendar_id.clone(),
                    account_id: AccountId("birthdays".to_string()),
                    display_name: birthdays.display_name.clone(),
                    color: None,
                    href: String::new(),
                    // Events-only, like feeds - never a task target.
                    supports_tasks: false,
                }],
                status: None,
            });
        }
    }
    groups.sort_by_key(|g| g.display_name.to_lowercase());
    let all_calendars: Vec<CalendarInfo> = groups.iter().flat_map(|g| g.calendars.iter().cloned()).collect();
    {
        let mut st = calendar_state.borrow_mut();
        ensure_checked_calendars(&mut st.checked_calendar_ids, &all_calendars);
        calendar_colors::assign_missing(&mut st.calendar_colors, &all_calendars);
    }
    let checked = calendar_state.borrow().checked_calendar_ids.clone();
    let colors = calendar_state.borrow().calendar_colors.clone();
    let on_toggle = {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        move |id: CalendarId, is_checked: bool| {
            {
                let mut st = calendar_state.borrow_mut();
                if is_checked {
                    st.checked_calendar_ids.insert(id);
                } else {
                    st.checked_calendar_ids.remove(&id);
                }
            }
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
        }
    };
    calendar_view::rebuild_calendar_checklist(calendar_list_box, &groups, &checked, &colors, &calendar_main.check_colors, on_toggle);
    calendar_view::set_calendar_colors(calendar_main, &colors);
    refresh_displayed_calendar_view(calendar_state, calendar_main);
}

/// Unions every connected calendar account's latest occurrences for
/// whatever month is currently displayed - filtered to only the calendars
/// currently checked in the sidebar - and redraws the whole main panel (every
/// view in the stack gets the merged set; each derives its own window from
/// the anchor). Same "only apply if it matches what's on screen" + "merge all
/// accounts' latest snapshot" approach as Mail's
/// `MessagesUpdated`/`rebuild_folder_tree`.
fn refresh_displayed_calendar_view(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &CalendarMain) {
    let mut st = calendar_state.borrow_mut();
    let month = st.displayed_month;
    let mut merged: Vec<EventOccurrence> = st.checked_occurrences(month).into_iter().cloned().collect();
    apply_pending_calendar_moves(&mut merged, &mut st.pending_calendar_moves);
    drop(st);
    calendar_view::set_occurrences(calendar_main, &merged);
}

/// Overwrites each occurrence in `occurrences` with its pending drag-move's
/// `start`/`end`, if one exists for its `(uid, recurrence_id)` - unless the
/// occurrence's own start/end already match the pending value, meaning the
/// server has now confirmed the move, in which case the entry is dropped
/// from `pending` instead (self-clearing).
fn apply_pending_calendar_moves(occurrences: &mut [EventOccurrence], pending: &mut HashMap<(EventUid, Option<chrono::DateTime<chrono::Utc>>), EventOccurrence>) {
    if pending.is_empty() {
        return;
    }
    let mut confirmed = Vec::new();
    for occ in occurrences.iter_mut() {
        let key = (occ.uid.clone(), occ.recurrence_id);
        if let Some(patch) = pending.get(&key) {
            if occ.start == patch.start && occ.end == patch.end {
                confirmed.push(key);
            } else {
                occ.start = patch.start;
                occ.end = patch.end;
            }
        }
    }
    for key in confirmed {
        pending.remove(&key);
    }
}

/// Prints the currently-displayed calendar month as an agenda: one section
/// per day, each day's events sorted by start time (all-day first), with
/// calendar names as context. Uses the same merged occurrence set as
/// [`refresh_displayed_calendar_view`] - checked calendars only, whatever
/// month the user is looking at - so the printout matches the view.
fn print_calendar_month<T: IsA<gtk::Window>>(calendar_state: &Rc<RefCell<CalendarUiState>>, window: &T) {
    let st = calendar_state.borrow();
    let month = st.displayed_month;
    let merged: Vec<EventOccurrence> = st.checked_occurrences(month).into_iter().cloned().collect();
    let mut calendar_names: std::collections::HashMap<CalendarId, String> = std::collections::HashMap::new();
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            calendar_names.insert(calendar.id.clone(), calendar.display_name.clone());
        }
    }
    for handle in st.webcal_handles.values() {
        calendar_names.insert(handle.calendar_id.clone(), handle.display_name.clone());
    }
    if let Some(birthdays) = &st.birthdays {
        calendar_names.insert(birthdays.calendar_id.clone(), birthdays.display_name.clone());
    }
    let month_start = month.with_day(1).unwrap();
    let month_end = (month_start + chrono::Months::new(1)) - chrono::Duration::days(1);
    let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, Vec<EventOccurrence>> = std::collections::BTreeMap::new();
    for occ in &merged {
        for day in calendar_view::covered_local_dates(occ, month_start, month_end) {
            by_day.entry(day).or_default().push(occ.clone());
        }
    }
    drop(st);
    let esc = |s: &str| gtk::glib::markup_escape_text(s).to_string();
    let mut sections = String::new();
    let mut day = month_start;
    while day <= month_end {
        if let Some(events) = by_day.get(&day) {
            let mut sorted = events.clone();
            sorted.sort_by_key(|occ| (occ.all_day, occ.start));
            let mut items = String::new();
            for occ in sorted {
                let time = if occ.all_day {
                    "All day".to_string()
                } else {
                    format!(
                        "{} – {}",
                        occ.start.with_timezone(&chrono::Local).format("%H:%M"),
                        occ.end.with_timezone(&chrono::Local).format("%H:%M")
                    )
                };
                let summary = occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string());
                let calendar = calendar_names.get(&occ.calendar_id).cloned().unwrap_or_default();
                items.push_str(&format!(
                    "<li><span class=\"time\">{}</span><span class=\"title\">{}</span>{}</li>",
                    esc(&time),
                    esc(&summary),
                    if calendar.is_empty() {
                        String::new()
                    } else {
                        format!(" <span class=\"cal\">· {}</span>", esc(&calendar))
                    },
                ));
            }
            sections.push_str(&format!("<h2>{}</h2><ul>{}</ul>", esc(&day.format("%A, %B %-e, %Y").to_string()), items));
        }
        day += chrono::Duration::days(1);
    }
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <style>body {{ font-family: sans-serif; color: #1f2328; margin: 2em; }} \
         h1 {{ font-size: 1.5em; margin: 0 0 0.6em; }} \
         h2 {{ font-size: 1.1em; margin: 1.1em 0 0.3em; border-bottom: 1px solid #d0d7de; padding-bottom: 0.15em; }} \
         ul {{ list-style: none; margin: 0; padding: 0; }} \
         li {{ padding: 0.15em 0; }} \
         .time {{ display: inline-block; min-width: 9em; color: #57606a; font-variant-numeric: tabular-nums; }} \
         .title {{ font-weight: 600; }} \
         .cal {{ color: #57606a; }}</style>\
         </head><body><h1>{}</h1>{}</body></html>",
        esc(&month_start.format("%B %Y").to_string()),
        sections,
    );
    print_html_once(&html, window);
}

/// Fills the Mail-screen overview pane's day list with two sections: every
/// checked calendar's occurrences (from whatever's currently cached - no new
/// fetch here) whose local date matches `day`, sorted by start time, and
/// every uncompleted task from every source (CalDAV, Google Tasks, local) -
/// tasks aren't day-scoped, since most have no due date at all, so the whole
/// outstanding set appears regardless of which day the mini-calendar shows,
/// in the same bucket order the Lookout dashboard's task section uses. The
/// event section is headed by `day`'s date ("Today"/"Tomorrow"/"Tue 12 Aug")
/// and shows a "No events" placeholder when there are none; the task section
/// shows a "No outstanding tasks" placeholder when the set is empty. Task
/// rows are the Tasks view's own rows, captioned to match the event list
/// and without the completion checkbox - clicking a row opens the shared
/// task editor - with the click handler `build_window` registered on
/// `calendar_state`. Unlike
/// `refresh_displayed_calendar_view`, the event filter is by exact day
/// rather than by the main Calendar view's own displayed month - the
/// overview pane can be showing a day from a different month entirely.
fn refresh_mail_overview_day_list(calendar_state: &Rc<RefCell<CalendarUiState>>, day: chrono::NaiveDate, day_list_box: &gtk::Box) {
    while let Some(child) = day_list_box.first_child() {
        day_list_box.remove(&child);
    }

    let (occurrences, tasks, colors, activate) = {
        let st = calendar_state.borrow();
        let occurrences: Vec<EventOccurrence> = st
            .checked_occurrences_all_months()
            .into_iter()
            .filter(|occ| !calendar_view::covered_local_dates(occ, day, day).is_empty())
            .cloned()
            .collect();
        let mut occurrences = occurrences;
        occurrences.sort_by_key(|occ| occ.start);
        // The `merged_tasks` union, inlined here so everything reads from
        // one borrow of `calendar_state`.
        let tasks: Vec<CalendarTask> = st
            .accounts
            .values()
            .flat_map(|h| h.last_tasks.iter().cloned())
            .chain(st.google_tasks.values().flat_map(|h| h.last_tasks.iter().cloned()))
            .chain(st.local_tasks.iter().cloned())
            .collect();
        let colors = st.calendar_colors.clone();
        let activate = st.mail_overview_activate.clone().unwrap_or_else(|| Rc::new(|_t| {}));
        (occurrences, tasks, colors, activate)
    };

    let header = gtk::Label::builder()
        .label(calendar_view::agenda_day_header(day, chrono::Utc::now().date_naive()))
        .css_classes(["caption-heading"])
        .xalign(0.0)
        .build();
    day_list_box.append(&header);

    if occurrences.is_empty() {
        let placeholder = gtk::Label::builder().label("No events").css_classes(["dim-label", "caption"]).xalign(0.0).build();
        day_list_box.append(&placeholder);
    } else {
        // A grid rather than one label per row: it auto-sizes the prefix
        // column to fit "All Day" (wider than any "HH:MM"), so every row's
        // title starts at the same x regardless of which prefix it has.
        let grid = gtk::Grid::builder().row_spacing(10).column_spacing(6).build();
        for (row, occ) in occurrences.into_iter().enumerate() {
            let row = row as i32;
            let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or(calendar_colors::DEFAULT_CHECK_COLOR).to_string();
            let dot = gtk::DrawingArea::builder().width_request(8).height_request(8).valign(gtk::Align::Center).build();
            dot.set_draw_func(move |_, cr, width, height| {
                let (r, g, b) = crate::tasks_view::parse_css_color(&color);
                let radius = width.min(height) as f64 / 2.0 - 1.0;
                cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius, 0.0, 2.0 * std::f64::consts::PI);
                cr.set_source_rgba(r, g, b, 1.0);
                let _ = cr.fill();
            });
            dot.set_tooltip_text(Some(&occ.calendar_id.0));
            let prefix_text = if occ.all_day {
                "All Day".to_string()
            } else {
                occ.start.with_timezone(&chrono::Local).format("%H:%M").to_string()
            };
            let prefix = gtk::Label::builder().label(&prefix_text).xalign(0.0).css_classes(["dim-label", "caption"]).build();
            let title = gtk::Label::builder()
                .label(occ.summary.as_deref().unwrap_or("(untitled)"))
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption"])
                .build();
            grid.attach(&dot, 0, row, 1, 1);
            grid.attach(&prefix, 1, row, 1, 1);
            grid.attach(&title, 2, row, 1, 1);
        }
        day_list_box.append(&grid);
    }

    let tasks_header = gtk::Label::builder()
        .label("Outstanding tasks")
        .css_classes(["caption-heading"])
        .xalign(0.0)
        .margin_top(8)
        .build();
    day_list_box.append(&tasks_header);

    let outstanding = crate::lookout_view::outstanding_tasks(&tasks, chrono::Local::now().naive_local(), usize::MAX);
    if outstanding.is_empty() {
        let placeholder = gtk::Label::builder()
            .label("No outstanding tasks")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .build();
        day_list_box.append(&placeholder);
    } else {
        for task in outstanding {
            day_list_box.append(&crate::tasks_view::task_row(&task, &colors, Rc::new(|_t, _c| {}), activate.clone(), &["caption"], false));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_calendar_account(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    reminders_engine: Rc<RefCell<crate::reminders::ReminderEngine>>,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
    account: GoaCalendarAccount,
    tasks_view: Rc<crate::tasks_view::TasksView>,
) {
    let account_id = account.account_id.clone();
    let display_name = account.display_name.clone();
    let config = CalendarAccountConfig {
        account_id: account_id.clone(),
        display_name: display_name.clone(),
        base_url: account.uri.clone(),
        accept_ssl_errors: account.accept_ssl_errors,
        // `Account.Identity` is the login username the CalDAV server expects
        // (e.g. the Nextcloud user id); the display name is only a fallback
        // for providers that don't advertise an Identity.
        username: if account.identity.is_empty() { display_name.clone() } else { account.identity.clone() },
    };
    let credentials: Rc<dyn lookout_dav::session::CalendarCredentialProvider> = Rc::new(GoaCalendarCredentialProvider::new(goa_client, account));
    // Same unsafe Send/Sync wrapper pattern as `connect_account`, documented
    // there - `run_calendar_session` requires `Arc<dyn
    // CalendarCredentialProvider>`, but the provider is only ever driven
    // from this one worker task.
    struct SendWrapper(Rc<dyn lookout_dav::session::CalendarCredentialProvider>);
    unsafe impl Send for SendWrapper {}
    unsafe impl Sync for SendWrapper {}
    #[async_trait::async_trait]
    impl lookout_dav::session::CalendarCredentialProvider for SendWrapper {
        async fn calendar_credential(&self) -> Result<lookout_dav::Credential, String> {
            self.0.calendar_credential().await
        }
    }
    let credentials: std::sync::Arc<dyn lookout_dav::session::CalendarCredentialProvider> = std::sync::Arc::new(SendWrapper(credentials));

    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    calendar_state.borrow_mut().accounts.insert(
        account_id.clone(),
        CalendarAccountHandle {
            cmd_tx,
            display_name,
            uri: config.base_url.clone(),
            calendars: Vec::new(),
            connection_state: CalConnectionState::Connecting,
            last_occurrences: Vec::new(),
            last_synced_month: None,
            occurrences_by_month: HashMap::new(),
            last_tasks: Vec::new(),
        },
    );

    worker.spawn(lookout_dav::session::run_calendar_session(config, credentials, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing
            // whole-snapshot events (calendars, a month's occurrences, tasks)
            // into the last copy of each - the fast-paint cache replay and
            // the first live sync queue the same snapshots back to back.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_calendar_events(batch) {
                match event {
                    CalendarSessionEvent::ConnectionStateChanged(state) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.connection_state = state.clone();
                        }
                        if let CalConnectionState::Error { message, retryable } = &state {
                            // Retryable failures are warnings (the session
                            // reconnects itself with backoff); the account's
                            // sidebar status text still shows the message, but no
                            // toast spams on every attempt. Only non-retryable
                            // (fatal) errors surface.
                            if !retryable {
                                let title = glib::markup_escape_text(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id)));
                                toast_overlay.add_toast(adw::Toast::new(&title));
                            }
                        }
                        refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                    }
                    CalendarSessionEvent::CalendarsUpdated(calendars) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.calendars = calendars.clone();
                        }
                        refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                        if let Some(handle) = calendar_state.borrow().accounts.get(&account_id) {
                            if !handle.calendars.is_empty() {
                                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(calendar_state.borrow().displayed_month));
                            }
                        }
                    }
                    CalendarSessionEvent::OccurrencesUpdated { month, occurrences } => {
                        // Feed the reminder engine before `occurrences` moves into
                        // the handle below - the engine keeps its own copy of
                        // every month it's been shown, since `last_occurrences`
                        // only holds whatever month synced last.
                        reminders_engine.borrow_mut().ingest(&account_id, &occurrences);
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.last_occurrences = occurrences;
                            handle.last_synced_month = Some(month);
                            insert_dashboard_occurrences(&mut handle.occurrences_by_month, month, handle.last_occurrences.clone());
                        }
                        refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                        refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                        // The dashboard's upcoming-events section follows the
                        // synced occurrences.
                        refresh_dashboard_hook(&calendar_state);
                        // The sidebar mini-calendar's bold event-day numerals track
                        // the currently-displayed month - refresh them when a fetch
                        // for that month lands (navigating to an uncached month
                        // marks its days bold the moment the sync completes).
                        if calendar_view::mini_month(&mini_calendar) == month {
                            let event_days = calendar_event_days(&calendar_state, month);
                            calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                        }
                    }
                    CalendarSessionEvent::Error(message) => {
                        let title = glib::markup_escape_text(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id)));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    CalendarSessionEvent::EventSaveFailed { uid, recurrence_id, message } => {
                        // The save actually failed server-side, so drop whatever
                        // optimistic drag-move was pending for this occurrence (a
                        // no-op if this failure didn't come from a drag) and
                        // repaint - the chip must snap back to its real,
                        // unmoved position rather than staying stuck showing the
                        // unsaved drop location.
                        calendar_state.borrow_mut().pending_calendar_moves.remove(&(uid, recurrence_id));
                        refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                        let title = glib::markup_escape_text(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id)));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    CalendarSessionEvent::TasksUpdated(tasks) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.last_tasks = tasks;
                        }
                        refresh_tasks_view(&calendar_state, &tasks_view);
                    }
                }
            }
        }
    });
}

/// The synthetic calendar id a subscription's events carry
/// (`"webcal:<subscription id>"`) - mirrors the `lookout-dav` session's
/// construction, so the two never drift.
fn webcal_calendar_id(subscription_id: &str) -> CalendarId {
    CalendarId(format!("webcal:{subscription_id}"))
}

/// Starts the single webcal feed session: loads the configured subscriptions
/// into `calendar_state`, spawns the polling actor on the worker, and routes
/// its `SubscriptionsUpdated` events into the same funnel as CalDAV
/// `OccurrencesUpdated` (checklist, main view merge, mail-overview day list,
/// mini-calendar event days). Runs at startup regardless of GOA - feeds are
/// not GOA accounts, and one session polls all of them.
#[allow(clippy::too_many_arguments)]
fn spawn_webcal_session(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    app_config: Rc<RefCell<crate::app_config::AppConfig>>,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    toast_overlay: adw::ToastOverlay,
) {
    let subscriptions = app_config.borrow().webcal_subscriptions.clone();
    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    {
        let mut st = calendar_state.borrow_mut();
        st.webcal_cmd_tx = Some(cmd_tx);
        st.webcal_subscriptions = subscriptions.clone();
        for sub in &subscriptions {
            st.webcal_handles.insert(
                sub.id.clone(),
                WebcalHandle {
                    calendar_id: webcal_calendar_id(&sub.id),
                    display_name: sub.display_name.clone(),
                    last_occurrences: Vec::new(),
                    last_synced_month: None,
                    occurrences_by_month: HashMap::new(),
                    error: None,
                },
            );
        }
    }

    worker.spawn(lookout_dav::subscription::run_subscription_session(subscriptions, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing each
            // month's feeds into the last update - the poll can queue several
            // months back to back, and each one repaints the calendar.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_subscription_events(batch) {
                let SubscriptionSessionEvent::SubscriptionsUpdated { month, feeds } = event;
                let mut error_toasts: Vec<String> = Vec::new();
                {
                    let mut st = calendar_state.borrow_mut();
                    for feed in feeds {
                        let Some(handle) = st.webcal_handles.get_mut(&feed.subscription_id) else { continue };
                        handle.last_occurrences = feed.occurrences;
                        handle.last_synced_month = Some(month);
                        insert_dashboard_occurrences(&mut handle.occurrences_by_month, month, handle.last_occurrences.clone());
                        // Toast on the *transition* into an error (or the first
                        // error for a fresh handle), not on every 5-minute poll.
                        if handle.error.is_none() && feed.error.is_some() {
                            error_toasts.push(handle.display_name.clone());
                        }
                        handle.error = feed.error;
                    }
                }
                for name in error_toasts {
                    let title = glib::markup_escape_text(&format!("Calendar feed \"{name}\" could not be fetched"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
                refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                // The dashboard's upcoming-events section follows webcal feeds too.
                refresh_dashboard_hook(&calendar_state);
                if calendar_view::mini_month(&mini_calendar) == month {
                    let event_days = calendar_event_days(&calendar_state, month);
                    calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                }
            }
        }
    });
}

fn calendar_account_label(state: &Rc<RefCell<CalendarUiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

/// The `(label, id)` list the event editor's calendar picker shows: every
/// discovered calendar across every connected account, labelled
/// "account · calendar" and sorted by account name.
fn pickable_calendars(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<(String, CalendarId)> {
    let st = calendar_state.borrow();
    let mut handles: Vec<&CalendarAccountHandle> = st.accounts.values().collect();
    handles.sort_by_key(|h| h.display_name.to_lowercase());
    let mut out = Vec::new();
    for handle in handles {
        for calendar in &handle.calendars {
            out.push((format!("{} · {}", handle.display_name, calendar.display_name), calendar.id.clone()));
        }
    }
    out
}

/// The `(label, id)` list the task editor's calendar picker shows: like
/// [`pickable_calendars`], but restricted to sources that can actually hold
/// tasks - CalDAV calendars whose server advertises `VTODO` support
/// (Google's CalDAV is `VEVENT`-only and rejects task PUTs with HTTP 403),
/// plus every connected Google Tasks list ("<email> · <list>").
fn pickable_task_calendars(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<(String, CalendarId)> {
    let st = calendar_state.borrow();
    let mut handles: Vec<&CalendarAccountHandle> = st.accounts.values().collect();
    handles.sort_by_key(|h| h.display_name.to_lowercase());
    let mut out = Vec::new();
    for handle in handles {
        for calendar in &handle.calendars {
            if calendar.supports_tasks {
                out.push((format!("{} · {}", handle.display_name, calendar.display_name), calendar.id.clone()));
            }
        }
    }
    for handle in st.google_tasks.values() {
        for list in &handle.task_lists {
            out.push((format!("{} · {}", handle.email, list.title), google_tasks::google_task_calendar_id(&list.id)));
        }
    }
    out
}

/// The task editor's default target: the first checked CalDAV calendar that
/// supports tasks, else any task-capable CalDAV calendar, else the first
/// Google Tasks list.
fn default_pickable_task_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Option<CalendarId> {
    let st = calendar_state.borrow();
    let task_capable = |calendar: &CalendarInfo| calendar.supports_tasks;
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            if task_capable(calendar) && st.checked_calendar_ids.contains(&calendar.id) {
                return Some(calendar.id.clone());
            }
        }
    }
    if let Some(calendar) = st.accounts.values().find_map(|handle| handle.calendars.iter().find(|c| task_capable(c))) {
        return Some(calendar.id.clone());
    }
    st.google_tasks
        .values()
        .find_map(|handle| handle.task_lists.first().map(|l| google_tasks::google_task_calendar_id(&l.id)))
}

/// The "Add as Task" mail toolbar button's default target: the preferred
/// account's own first task-capable CalDAV calendar (looked up by
/// `AccountId`, the id space `CalendarUiState::accounts` shares with a
/// GOA-connected mail account) or Google Tasks list (looked up by email,
/// the id `CalendarUiState::google_tasks` is keyed by since a Google Tasks
/// connection isn't necessarily tied to a GOA calendar account) - so a task
/// created from an email defaults into that email's own account rather than
/// an arbitrary one. Falls back to `default_pickable_task_calendar`'s
/// ordering, then finally the local on-device list.
fn default_task_calendar_preferring(calendar_state: &Rc<RefCell<CalendarUiState>>, preferred_account: Option<&AccountId>, preferred_email: Option<&str>) -> CalendarId {
    let st = calendar_state.borrow();
    if let Some(preferred_account) = preferred_account {
        if let Some(calendar) = st.accounts.get(preferred_account).and_then(|h| h.calendars.iter().find(|c| c.supports_tasks)) {
            return calendar.id.clone();
        }
    }
    if let Some(preferred_email) = preferred_email {
        if let Some(list) = st.google_tasks.get(preferred_email).and_then(|h| h.task_lists.first()) {
            return google_tasks::google_task_calendar_id(&list.id);
        }
    }
    drop(st);
    default_pickable_task_calendar(calendar_state).unwrap_or_else(local_tasks_calendar_id)
}

/// The editor's default calendar for a new event: the first checked calendar
/// (the one whose events are actually on screen), or any calendar if nothing
/// is checked.
fn default_pickable_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Option<CalendarId> {
    let st = calendar_state.borrow();
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            if st.checked_calendar_ids.contains(&calendar.id) {
                return Some(calendar.id.clone());
            }
        }
    }
    st.accounts.values().find_map(|handle| handle.calendars.first().map(|c| c.id.clone()))
}

/// Best-effort address for the account owning `calendar_id`, used as
/// `ORGANIZER` when an event ends up with attendees. GOA calendar accounts
/// carry no dedicated email field, but their display name is commonly *is*
/// the account's email address (Google/Nextcloud/Fastmail all populate it
/// that way) - accepted only when it actually looks like one, else `None`
/// (a documented non-conformance the event editor already tolerates).
fn calendar_owner_email(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<String> {
    let st = calendar_state.borrow();
    let display_name = st
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|c| c.id == *calendar_id))?
        .display_name
        .clone();
    crate::recipient_entry::is_plausible_address(&display_name).then_some(display_name)
}

/// A read-only snapshot for the event editor's right-hand preview panel:
/// every occurrence from checked calendars in the month containing `anchor`,
/// which local dates in that month have events, and the calendar-color map -
/// the same filtering `calendar_event_days`/`refresh_mail_overview_day_list`
/// already do for the sidebar's own mini-calendar. The editor never fetches
/// anything itself, so this is exactly what it can show.
struct EventEditorPreviewData {
    occurrences: Vec<EventOccurrence>,
    month_event_days: HashSet<chrono::NaiveDate>,
    colors: calendar_colors::CalendarColorMap,
}

fn event_editor_preview_data(calendar_state: &Rc<RefCell<CalendarUiState>>, anchor: chrono::NaiveDate) -> EventEditorPreviewData {
    let occurrences: Vec<EventOccurrence> = {
        let st = calendar_state.borrow();
        st.checked_occurrences_all_months().into_iter().cloned().collect()
    };
    let month_event_days = calendar_event_days(calendar_state, first_of_month(anchor));
    let colors = calendar_state.borrow().calendar_colors.clone();
    EventEditorPreviewData {
        occurrences,
        month_event_days,
        colors,
    }
}

/// Opens the editor for an existing occurrence — the shared path behind
/// clicking an event chip in the calendar grids and the notification
/// reminder's Open action (the engine hands the full occurrence back, so the
/// editor works even when the month it belongs to isn't the one on screen).
///
/// An occurrence from a read-only synthetic calendar (a webcal subscription,
/// or the birthdays calendar) opens the editor in read-only mode - neither
/// has a write-back path - so every input is disabled and the save/delete
/// actions are hidden, with a dim note explaining the source.
fn open_event_editor_for(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, calendar_state: &Rc<RefCell<CalendarUiState>>, occ: &EventOccurrence) {
    let (read_only, read_only_note) = {
        let st = calendar_state.borrow();
        let note = st.read_only_note(&occ.calendar_id);
        (note.is_some(), note.unwrap_or(""))
    };
    let calendars = pickable_calendars(calendar_state);
    let default_calendar = if read_only {
        occ.calendar_id.clone()
    } else {
        if calendars.is_empty() {
            return;
        }
        if calendars.iter().any(|(_, id)| *id == occ.calendar_id) {
            occ.calendar_id.clone()
        } else {
            default_pickable_calendar(calendar_state).unwrap_or_else(|| CalendarId(String::new()))
        }
    };
    let preview = event_editor_preview_data(calendar_state, occ.start.with_timezone(&chrono::Local).date_naive());
    let owner_email = calendar_owner_email(calendar_state, &occ.calendar_id);
    crate::event_editor::show_event_editor(
        window,
        crate::event_editor::EventEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: Some(occ),
            suggested_start: None,
            suggested_end: None,
            month_occurrences: &preview.occurrences,
            month_event_days: &preview.month_event_days,
            calendar_colors: &preview.colors,
            owner_email,
            read_only,
            read_only_note,
        },
        calendar_attendee_suggestions(state, worker),
        {
            let calendar_state = calendar_state.clone();
            move |calendar_id, event| route_calendar_save(&calendar_state, calendar_id, event)
        },
        {
            let calendar_state = calendar_state.clone();
            move |calendar_id, occ| route_calendar_delete(&calendar_state, calendar_id, occ)
        },
    );
}

/// Opens the "New event" editor prefilled for `suggested_start` (and
/// optionally `suggested_end`), shared by the Calendar toolbar's New Event
/// button, the Day/Week grids' highlighted slot ranges, and month-grid day
/// activation so every entry point behaves identically (a connect-a-calendar
/// toast when nothing writable exists, the editor otherwise).
fn show_new_event_editor(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    suggested_start: chrono::NaiveDateTime,
    suggested_end: Option<chrono::NaiveDateTime>,
) {
    let calendars = pickable_calendars(calendar_state);
    let Some(default_calendar) = default_pickable_calendar(calendar_state) else {
        toast_overlay.add_toast(adw::Toast::new("Connect a calendar account to create events."));
        return;
    };
    let preview = event_editor_preview_data(calendar_state, suggested_start.date());
    let owner_email = calendar_owner_email(calendar_state, &default_calendar);
    crate::event_editor::show_event_editor(
        window,
        crate::event_editor::EventEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: None,
            suggested_start: Some(suggested_start),
            suggested_end,
            month_occurrences: &preview.occurrences,
            month_event_days: &preview.month_event_days,
            calendar_colors: &preview.colors,
            owner_email,
            read_only: false,
            read_only_note: "",
        },
        calendar_attendee_suggestions(state, worker),
        {
            let calendar_state = calendar_state.clone();
            move |calendar_id, event| route_calendar_save(&calendar_state, calendar_id, event)
        },
        {
            let calendar_state = calendar_state.clone();
            move |calendar_id, occ| route_calendar_delete(&calendar_state, calendar_id, occ)
        },
    );
}

/// The account session that owns `calendar_id`, so its command channel can
/// carry create/update/delete writes.
fn calendar_handle_for_id(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<async_channel::Sender<CalendarCommand>> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|c| c.id == *calendar_id))
        .map(|handle| handle.cmd_tx.clone())
}

/// Routes a saved (created or edited) event to its account's session. An event
/// with a `href` is an edit in place (`UpdateEvent`, guarded by its etag); one
/// without is a brand-new event (`CreateEvent`, fresh `<uid>.ics` href). The
/// session resyncs on success, which repaints the views.
fn route_calendar_save(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: CalendarId, event: CalendarEvent) {
    let Some(handle) = calendar_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to save an event into an unknown calendar {calendar_id}");
        return;
    };
    if event.href.is_some() {
        let _ = handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(event) });
    } else {
        let _ = handle.send_blocking(CalendarCommand::CreateEvent { event: Box::new(event) });
    }
}

/// Routes an event deletion to its account's session, honoring the event's
/// recurrence structure: an override (or a non-recurring event) deletes its
/// own resource; a plain expansion of a recurring series instead adds its
/// instance time to the master's EXDATEs (via the etag-guarded
/// `UpdateEvent`), removing exactly that instance without touching the rest
/// of the series.
fn route_calendar_delete(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: CalendarId, occ: EventOccurrence) {
    if calendar_state.borrow().is_read_only_calendar(&occ.calendar_id) {
        tracing::warn!("refusing to delete a read-only subscription event");
        return;
    }
    let Some(handle) = calendar_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to delete an event from an unknown calendar {calendar_id}");
        return;
    };
    if occ.recurrence_id.is_some() {
        // A per-occurrence override: delete its own resource.
        let Some(href) = occ.href else {
            tracing::warn!("tried to delete an override event without a server href");
            return;
        };
        let _ = handle.send_blocking(CalendarCommand::DeleteEvent {
            calendar_id,
            href,
            etag: occ.etag,
        });
        return;
    }
    if occ.rrule.is_some() {
        // A plain instance of a recurring series: EXDATE it out of the
        // master. The occurrence already carries the master's fields -
        // its href/etag are the master's resource, and its exdates list is
        // the master's - so the only change is pushing the instance time.
        let mut master = CalendarEvent {
            uid: occ.uid.clone(),
            calendar_id: occ.calendar_id.clone(),
            summary: occ.summary.clone(),
            description: occ.description.clone(),
            location: occ.location.clone(),
            start: occ.start,
            end: occ.end,
            all_day: occ.all_day,
            rrule: occ.rrule.clone(),
            recurrence_id: None,
            recurrence_range: lookout_core::RecurrenceRange::default(),
            exdates: occ.exdates.clone(),
            rdates: Vec::new(),
            href: occ.master_href.clone().or_else(|| occ.href.clone()),
            etag: occ.master_etag.clone().or_else(|| occ.etag.clone()),
            attendees: occ.attendees.clone(),
            organizer: occ.organizer.clone(),
            categories: occ.categories.clone(),
            sensitivity: occ.sensitivity,
            transparency: occ.transparency,
            reminder_minutes_before: occ.reminder_minutes_before,
            conference_url: occ.conference_url.clone(),
        };
        master.exdates.push(occ.start);
        let _ = handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(master) });
        return;
    }
    // A standalone non-recurring event: delete its resource outright.
    let Some(href) = occ.href else {
        tracing::warn!("tried to delete an event without a server href");
        return;
    };
    let _ = handle.send_blocking(CalendarCommand::DeleteEvent {
        calendar_id,
        href,
        etag: occ.etag,
    });
}

/// The account session that owns `calendar_id`, for task writes - the
/// `calendar_handle_for_id` equivalent (same "calendar collection belongs to
/// its account's handle" lookup).
fn task_handle_for_id(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId) -> Option<async_channel::Sender<CalendarCommand>> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .find(|handle| handle.calendars.iter().any(|c| c.id == *calendar_id))
        .map(|handle| handle.cmd_tx.clone())
}

/// The synthetic calendar id of the on-device-only task store (plan C: the
/// fallback when no connected source supports tasks). Tasks with this id
/// live in the UI-state database and never sync anywhere.
fn local_tasks_calendar_id() -> CalendarId {
    CalendarId("local".to_string())
}

/// The Google Tasks session owning a `googletasks:<list id>` calendar, if
/// any - the task-write routing lookup.
fn google_tasks_handle_for_calendar<'a>(state: &'a CalendarUiState, calendar_id: &CalendarId) -> Option<&'a GoogleTasksHandle> {
    let list_id = google_tasks::google_task_list_id(calendar_id)?;
    state.google_tasks.values().find(|handle| handle.task_lists.iter().any(|l| l.id == list_id))
}

/// Unions every connected task source's latest snapshot: CalDAV accounts'
/// tasks, Google Tasks accounts' tasks, and the local on-device store.
/// Tasks have no month window, so the full set is always the merge unit.
fn merged_tasks(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<CalendarTask> {
    let st = calendar_state.borrow();
    st.accounts
        .values()
        .flat_map(|handle| handle.last_tasks.iter().cloned())
        .chain(st.google_tasks.values().flat_map(|handle| handle.last_tasks.iter().cloned()))
        .chain(st.local_tasks.iter().cloned())
        .collect()
}

/// Repaints the Tasks view from every source's latest snapshot, setting the
/// empty-state message to match what task sources exist (B: "you have no
/// tasks" vs "you have nowhere to put tasks").
fn refresh_tasks_view(calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>) {
    let colors = calendar_state.borrow().calendar_colors.clone();
    let tasks = merged_tasks(calendar_state);
    let st = calendar_state.borrow();
    let has_caldav_tasks = st.accounts.values().flat_map(|h| h.calendars.iter()).any(|c| c.supports_tasks);
    let has_google_tasks = !st.google_tasks.is_empty();
    drop(st);
    let message = if has_caldav_tasks || has_google_tasks {
        "No tasks yet - use New task to create one.".to_string()
    } else {
        "No connected calendar supports tasks, so new tasks are saved on this device only. To sync tasks, use Connect Google Tasks or connect a calendar that supports them (Nextcloud, iCloud).".to_string()
    };
    crate::tasks_view::set_empty_message(tasks_view, &message);
    crate::tasks_view::set_tasks(tasks_view, &tasks, &colors);
    // Every task change funnels through here (CalDAV and Google Tasks
    // `TasksUpdated` events, local saves, checkbox flips), so this is the
    // single point that keeps the Lookout dashboard's task section, the
    // mail toolbar's "Add as Task" flag button's icon, and the Mail-screen
    // overview pane's task list live.
    refresh_dashboard_hook(calendar_state);
    refresh_task_button_hook(calendar_state);
    refresh_mail_overview_hook(calendar_state);
}

/// Runs the Lookout dashboard's repaint hook if the window has registered
/// one - a no-op in tests and before the window finishes wiring, never an
/// error.
fn refresh_dashboard_hook(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let hook = calendar_state.borrow().dashboard_refresh.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Runs the mail toolbar's "Add as Task" flag button's repaint hook if the
/// window has registered one - same no-op-until-wired convention as
/// `refresh_dashboard_hook`.
fn refresh_task_button_hook(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let hook = calendar_state.borrow().task_button_refresh.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Runs the Mail-screen overview pane's repaint hook if the window has
/// registered one - same no-op-until-wired convention as
/// `refresh_dashboard_hook`.
fn refresh_mail_overview_hook(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let hook = calendar_state.borrow().mail_overview_refresh.clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Repaints the Lookout dashboard from the current mail caches and
/// calendar state: most-contacted people and the hour-of-day histogram are
/// read straight off each connected account's cache SQLite (the same WAL
/// reader the composer autocomplete uses, so no session round trip), and
/// tasks/events come from the in-memory calendar state.
fn refresh_lookout_view(state: &Rc<RefCell<UiState>>, calendar_state: &Rc<RefCell<CalendarUiState>>, lookout_view: &Rc<crate::lookout_view::LookoutView>) {
    // Top contacts: union each account's top list, re-rank, keep the best.
    let mut contacts: Vec<(lookout_core::EmailAddress, i64)> = Vec::new();
    let mut histogram = [0i64; 24];
    for cache in state.borrow().accounts.values().filter_map(|h| h.address_cache.as_ref()) {
        if let Ok(top) = cache.top_addresses(crate::lookout_view::TOP_CONTACTS_LIMIT) {
            contacts.extend(top);
        }
        if let Ok(h) = cache.hour_histogram(None) {
            for (i, count) in h.iter().enumerate() {
                histogram[i] += count;
            }
        }
    }
    contacts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    contacts.truncate(crate::lookout_view::TOP_CONTACTS_LIMIT);

    let (tasks, events, checked_calendar_ids, colors) = {
        let st = calendar_state.borrow();
        (
            merged_tasks(calendar_state),
            // The dashboard unions *every* cached month (pruned to current +
            // next by `insert_dashboard_occurrences`), so its 14-day horizon
            // stays populated as events pass instead of draining to whatever
            // single month the sessions' polls last synced.
            st.accounts
                .values()
                .flat_map(|h| h.occurrences_by_month.values().flatten().cloned())
                .chain(st.webcal_handles.values().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
                .chain(st.birthdays.as_ref().into_iter().flat_map(|h| h.occurrences_by_month.values().flatten().cloned()))
                .collect(),
            st.checked_calendar_ids.clone(),
            st.calendar_colors.clone(),
        )
    };

    crate::lookout_view::set_data(
        lookout_view,
        &crate::lookout_view::LookoutData {
            contacts,
            histogram,
            tasks,
            events,
            checked_calendar_ids,
            colors,
        },
    );
}

/// Asks every calendar source to fetch the current and next month, so the
/// dashboard's "upcoming events" can reach past the month each session last
/// fetched on its 5-minute cadence. Uses the fetch-only `FetchMonth`
/// commands - deliberately *not* `SyncMonth` - so the sessions' polled
/// month (whatever the calendar view is showing) is left alone; hijacking
/// it would starve the mail-overview day list and the calendar tab.
fn widen_calendar_sync_horizon(calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let today = chrono::Local::now().date_naive();
    let months = [today, today + chrono::Months::new(1)];
    let mut st = calendar_state.borrow_mut();
    if let Some(birthdays) = &mut st.birthdays {
        birthdays.sync_dashboard_window();
    }
    for handle in st.accounts.values() {
        for month in months {
            let _ = handle.cmd_tx.send_blocking(CalendarCommand::FetchMonth(month));
        }
    }
    if let Some(cmd_tx) = &st.webcal_cmd_tx {
        for month in months {
            let _ = cmd_tx.send_blocking(SubscriptionCommand::FetchMonth(month));
        }
    }
}

/// The months the Lookout dashboard's events section cares about - the
/// current month and the next, which together cover its 14-day horizon.
fn dashboard_month_window() -> [chrono::NaiveDate; 2] {
    let current = chrono::Local::now().date_naive();
    [first_of_month(current), first_of_month(current) + chrono::Months::new(1)]
}

/// Stashes one synced month's occurrences in a dashboard map, pruning any
/// month outside the current/next window so a long-lived session never
/// accumulates stale month buckets.
fn insert_dashboard_occurrences(map: &mut HashMap<chrono::NaiveDate, Vec<EventOccurrence>>, month: chrono::NaiveDate, occurrences: Vec<EventOccurrence>) {
    map.insert(month, occurrences);
    let window = dashboard_month_window();
    map.retain(|m, _| *m >= window[0] && *m <= window[1]);
}

/// Routes a completion-toggle (list checkbox) to the task's store: the
/// checkbox already rewrote the status/completed/percent fields, so this is
/// a plain update-in-place.
fn route_task_toggle(calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>, task: CalendarTask, _completed: bool) {
    route_task_save(calendar_state, tasks_view, task.calendar_id.clone(), task);
}

/// Opens the task editor for a brand-new task. The picker lists every
/// task-capable source; when none exists it falls back to a single "Local
/// (this device)" entry, so New Task always has somewhere to go.
fn show_new_task_editor(window: &adw::ApplicationWindow, calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>) {
    let mut calendars = pickable_task_calendars(calendar_state);
    if calendars.is_empty() {
        calendars.push(("Local (this device)".to_string(), local_tasks_calendar_id()));
    }
    let default_calendar = default_pickable_task_calendar(calendar_state).unwrap_or_else(local_tasks_calendar_id);
    let calendar_state = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    crate::task_editor::show_task_editor(
        window,
        crate::task_editor::TaskEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: None,
            prefill: None,
        },
        move |calendar_id, task| route_task_save(&calendar_state, &tasks_view, calendar_id, task),
        move |_calendar_id, _uid, _href, _etag| {},
    );
}

/// The hidden marker line appended to an "Add as Task" task's Notes so
/// `email_has_task` can later recognize which email spawned it - the only
/// channel that reliably round-trips through every task backend (CalDAV's
/// `DESCRIPTION` and Google Tasks' `notes` both preserve free text as-is,
/// but Google Tasks drops `categories` entirely - see `task_to_write` - so a
/// tag stored there wouldn't survive a Google Tasks round trip).
fn task_email_marker(message_id: &str) -> String {
    format!("Lookout-Message-Id: {message_id}")
}

/// Whether any known task (local, CalDAV, or Google Tasks) was created from
/// the email carrying `message_id` - drives the "Add as Task" flag button's
/// filled/outline icon.
fn email_has_task(calendar_state: &Rc<RefCell<CalendarUiState>>, message_id: &str) -> bool {
    let marker = task_email_marker(message_id);
    merged_tasks(calendar_state)
        .iter()
        .any(|task| task.description.as_deref().is_some_and(|d| d.contains(&marker)))
}

/// Sets the mail toolbar's "Add as Task" flag button's icon to reflect
/// whether the currently selected message already has an associated task -
/// filled/solid when `email_has_task` finds one, outline otherwise
/// (including when nothing, or more than one message, is selected, since
/// `selected_summary()` is `None` for both). Mirrors
/// `refresh_mark_read_button`'s icon-swap convention.
fn refresh_task_button(button: &gtk::Button, message_list: &MessageListModel, calendar_state: &Rc<RefCell<CalendarUiState>>) {
    let has_task = message_list
        .selected_summary()
        .and_then(|summary| summary.message_id)
        .is_some_and(|message_id| email_has_task(calendar_state, &message_id));
    let icon = if has_task {
        themed_icon_name(&["mail-flag-symbolic", "flag-filled-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["flag-outline-thin-symbolic", "flag-outline-symbolic", "mail-mark-important-symbolic"])
    };
    button.set_icon_name(icon);
}

/// Opens the task editor's create form seeded from a mail message - the mail
/// toolbar's "Add as Task" button. The title is the email's subject
/// (falling back to "(no subject)"); since `CalendarTask` has no url/link
/// field, the Notes field carries a "From: <sender> — <date>" line as the
/// only way back to the source email. The calendar/list picker defaults to
/// `preferred_account`'s own task-capable calendar or Google Tasks list when
/// it has one, else the same fallback order `show_new_task_editor` uses,
/// else "Local (this device)".
fn show_create_task_for_email(
    window: &adw::ApplicationWindow,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    summary: &EmailSummary,
    preferred_account: Option<AccountId>,
    preferred_email: Option<String>,
) {
    let mut calendars = pickable_task_calendars(calendar_state);
    if calendars.is_empty() {
        calendars.push(("Local (this device)".to_string(), local_tasks_calendar_id()));
    }
    let default_calendar = default_task_calendar_preferring(calendar_state, preferred_account.as_ref(), preferred_email.as_deref());

    let title = summary.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(no subject)").to_string();
    let sender_label = summary
        .from
        .first()
        .map(|addr| addr.name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&addr.address).to_string())
        .unwrap_or_else(|| "unknown sender".to_string());
    let mut description = format!(
        "From: {sender_label} — {}",
        summary.date.with_timezone(&chrono::Local).format("%a, %b %-d, %Y at %-I:%M %p")
    );
    // A hidden marker line so `email_has_task` can later recognize this task
    // as belonging to this email, driving the flag button's filled icon -
    // skipped when the message carries no Message-ID (rare, but then there's
    // nothing stable to match against).
    if let Some(message_id) = &summary.message_id {
        description.push_str("\n\n");
        description.push_str(&task_email_marker(message_id));
    }

    let seed = CalendarTask {
        uid: TaskUid(uuid::Uuid::new_v4().to_string()),
        calendar_id: default_calendar.clone(),
        summary: Some(title),
        description: Some(description),
        due: None,
        start: None,
        completed: None,
        status: TaskStatus::NeedsAction,
        priority: TaskPriority(0),
        percent_complete: None,
        categories: Vec::new(),
        href: None,
        etag: None,
    };

    let calendar_state = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    crate::task_editor::show_task_editor(
        window,
        crate::task_editor::TaskEditorPrefill {
            calendars: &calendars,
            default_calendar,
            existing: None,
            prefill: Some(&seed),
        },
        move |calendar_id, task| route_task_save(&calendar_state, &tasks_view, calendar_id, task),
        move |_calendar_id, _uid, _href, _etag| {},
    );
}

/// Opens the editor for an existing task.
fn open_task_editor_for(window: &adw::ApplicationWindow, calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>, task: &CalendarTask) {
    // The picker lists every task-capable source. The task's own store is
    // appended unconditionally too: the save handler stamps the form's
    // selected calendar onto the task, so an edit whose source fell out of
    // the filtered list must still resolve back to it, not silently re-home
    // the task into the first picker entry.
    let mut calendars = pickable_task_calendars(calendar_state);
    if !calendars.iter().any(|(_, id)| *id == task.calendar_id) {
        if task.calendar_id == local_tasks_calendar_id() {
            calendars.push(("Local (this device)".to_string(), task.calendar_id.clone()));
        } else if let Some((handle, info)) = calendar_state
            .borrow()
            .accounts
            .values()
            .find_map(|h| h.calendars.iter().find(|c| c.id == task.calendar_id).map(|c| (h, c)))
        {
            calendars.push((format!("{} · {}", handle.display_name, info.display_name), task.calendar_id.clone()));
        } else if let Some(list) = calendar_state.borrow().google_tasks.values().find_map(|h| {
            h.task_lists
                .iter()
                .find(|l| google_tasks::google_task_calendar_id(&l.id) == task.calendar_id)
                .map(|l| (h, l))
        }) {
            calendars.push((format!("{} · {}", list.0.email, list.1.title), task.calendar_id.clone()));
        }
    }
    let calendar_state = calendar_state.clone();
    let calendar_state_for_delete = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    let tasks_view_for_delete = tasks_view.clone();
    crate::task_editor::show_task_editor(
        window,
        crate::task_editor::TaskEditorPrefill {
            calendars: &calendars,
            default_calendar: task.calendar_id.clone(),
            existing: Some(task),
            prefill: None,
        },
        move |calendar_id, task| route_task_save(&calendar_state, &tasks_view, calendar_id, task),
        move |calendar_id, uid, href, etag| route_task_delete(&calendar_state_for_delete, &tasks_view_for_delete, calendar_id, uid, href, etag),
    );
}

/// Routes a saved (created or edited) task to its store - one of: the local
/// on-device store, the owning Google Tasks session, or the owning CalDAV
/// account's session (the `route_calendar_save` counterpart).
fn route_task_save(calendar_state: &Rc<RefCell<CalendarUiState>>, tasks_view: &Rc<crate::tasks_view::TasksView>, calendar_id: CalendarId, task: CalendarTask) {
    if calendar_id == local_tasks_calendar_id() {
        let mut st = calendar_state.borrow_mut();
        match st.local_tasks.iter_mut().find(|t| t.uid == task.uid) {
            Some(existing) => *existing = task.clone(),
            None => st.local_tasks.push(task.clone()),
        }
        if let Some(db) = &st.local_tasks_db {
            let _ = db.borrow().save_local_task(&task);
        }
        drop(st);
        refresh_tasks_view(calendar_state, tasks_view);
        return;
    }
    if let Some(list_id) = google_tasks::google_task_list_id(&calendar_id) {
        let (handle, is_new) = {
            let st = calendar_state.borrow();
            let Some(handle) = google_tasks_handle_for_calendar(&st, &calendar_id) else {
                tracing::warn!("tried to save a task into an unknown Google task list {calendar_id}");
                return;
            };
            (handle.cmd_tx.clone(), !handle.last_tasks.iter().any(|t| t.uid == task.uid))
        };
        let cmd = if is_new {
            GoogleTasksCommand::CreateTask {
                list_id: list_id.to_string(),
                task: Box::new(task),
            }
        } else {
            GoogleTasksCommand::UpdateTask {
                list_id: list_id.to_string(),
                task: Box::new(task),
            }
        };
        let _ = handle.send_blocking(cmd);
        return;
    }
    let Some(handle) = task_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to save a task into an unknown calendar {calendar_id}");
        return;
    };
    if task.href.is_some() {
        let _ = handle.send_blocking(CalendarCommand::UpdateTask { task: Box::new(task) });
    } else {
        let _ = handle.send_blocking(CalendarCommand::CreateTask { task: Box::new(task) });
    }
}

/// Routes a task deletion to its store. `uid` is the store's own key for
/// local/Google tasks; `href`/`etag` identify the CalDAV resource.
fn route_task_delete(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    calendar_id: CalendarId,
    uid: TaskUid,
    href: Option<String>,
    etag: Option<String>,
) {
    if calendar_id == local_tasks_calendar_id() {
        let mut st = calendar_state.borrow_mut();
        if let Some(db) = &st.local_tasks_db {
            let _ = db.borrow().delete_local_task(&uid.0);
        }
        st.local_tasks.retain(|t| t.uid != uid);
        drop(st);
        refresh_tasks_view(calendar_state, tasks_view);
        return;
    }
    if let Some(list_id) = google_tasks::google_task_list_id(&calendar_id) {
        let st = calendar_state.borrow();
        let Some(handle) = google_tasks_handle_for_calendar(&st, &calendar_id) else {
            tracing::warn!("tried to delete a task from an unknown Google task list {calendar_id}");
            return;
        };
        // A Google task's uid is its API task id.
        let _ = handle.cmd_tx.send_blocking(GoogleTasksCommand::DeleteTask {
            list_id: list_id.to_string(),
            task_id: uid.0,
        });
        return;
    }
    let Some(handle) = task_handle_for_id(calendar_state, &calendar_id) else {
        tracing::warn!("tried to delete a task from an unknown calendar {calendar_id}");
        return;
    };
    let Some(href) = href else {
        tracing::warn!("tried to delete a task without a server href");
        return;
    };
    let _ = handle.send_blocking(CalendarCommand::DeleteTask { calendar_id, href, etag });
}

/// The calendar object already stored under `event.uid`, if any - the iMIP
/// reply flows upsert against it instead of creating a duplicate.
fn find_calendar_occurrence(calendar_state: &Rc<RefCell<CalendarUiState>>, uid: &EventUid) -> Option<EventOccurrence> {
    let st = calendar_state.borrow();
    let occurrences = st
        .accounts
        .values()
        .flat_map(|handle| handle.last_occurrences.iter())
        .filter(|occurrence| occurrence.uid == *uid)
        .collect::<Vec<_>>();
    // Overrides share their master's UID; prefer a master (or any
    // non-recurring) occurrence so whole-event lookups (the iMIP upsert
    // path) don't accidentally resolve to a single-instance override.
    occurrences
        .iter()
        .find(|occ| occ.recurrence_id.is_none())
        .or_else(|| occurrences.first())
        .map(|occ| (*occ).clone())
}

/// The user's answer to an iMIP invitation (REQUEST): sends the RFC 6047
/// `METHOD:REPLY` email back to the organizer with the chosen `PARTSTAT`, and
/// best-effort saves the event into the account's default calendar, upserting
/// by UID so accepting a re-invitation updates the existing booking rather
/// than duplicating it. The reply is the primary action: a missing or busy
/// calendar setup is a toast, not a blocked send.
fn respond_to_imip_invitation(
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    toast_overlay: &adw::ToastOverlay,
    invitation: &lookout_core::ImipInvitation,
    from_email: &str,
    display_name: Option<&str>,
    cmd_tx: &async_channel::Sender<AccountCommand>,
    status: AttendeeStatus,
) {
    let Some(organizer) = &invitation.organizer else {
        toast_overlay.add_toast(adw::Toast::new("This invitation names no organizer - can't send a reply."));
        return;
    };
    let Some(mut event) = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), &invitation.ics).into_iter().next() else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this invitation's calendar data."));
        return;
    };
    // Stamp the user's own PARTSTAT onto their ATTENDEE line (the organizer's
    // client reads the reply from exactly this); a self-signed invitation
    // that omitted the recipient entirely gets an ATTENDEE added.
    match event.attendees.iter_mut().find(|attendee| attendee.address.address.eq_ignore_ascii_case(from_email)) {
        Some(attendee) => attendee.status = status,
        None => event.attendees.push(Attendee {
            address: EmailAddress::new(from_email.to_string()),
            role: AttendeeRole::Required,
            status,
        }),
    }
    let reply_ics = lookout_dav::build_imip_vcalendar(&event, lookout_core::ImipMethod::Reply);
    let (subject_prefix, toast) = match status {
        AttendeeStatus::Accepted => ("Accepted", "Invitation accepted"),
        AttendeeStatus::Tentative => ("Tentatively accepted", "Marked as tentative"),
        AttendeeStatus::Declined => ("Declined", "Invitation declined"),
        AttendeeStatus::NeedsAction => ("Replied", "Reply sent"),
    };
    let subject = match &invitation.summary {
        Some(summary) => format!("{subject_prefix}: {summary}"),
        None => subject_prefix.to_string(),
    };
    let message = lookout_mail::ComposedMessage {
        from: from_email.to_string(),
        display_name: display_name.map(str::to_string).filter(|n| !n.trim().is_empty()),
        to: vec![organizer.address.clone()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject,
        text_body: format!("{subject_prefix}."),
        html_body: None,
        attachments: vec![],
        inline_images: vec![],
        calendar_part: Some(reply_ics),
        read_receipt: None,
        request_read_receipt: false,
        in_reply_to: invitation.in_reply_to.clone(),
        references: vec![],
        message_id: None,
    };
    let _ = cmd_tx.send_blocking(AccountCommand::SendMessage(Box::new(message)));
    toast_overlay.add_toast(adw::Toast::new(toast));

    // Best-effort calendar write: the reply already went out, so a missing
    // calendar (or a failed save) is a warning, never a blocker.
    let Some(default_calendar) = default_pickable_calendar(calendar_state) else {
        toast_overlay.add_toast(adw::Toast::new("Connect a calendar account to save the event."));
        return;
    };
    match find_calendar_occurrence(calendar_state, &event.uid) {
        // A re-invitation updating an event already in the calendar: write
        // over the stored object in place.
        Some(existing) => {
            event.calendar_id = existing.calendar_id.clone();
            event.href = existing.href;
            event.etag = existing.etag;
            let handle = calendar_handle_for_id(calendar_state, &event.calendar_id);
            let _ = handle.map(|handle| handle.send_blocking(CalendarCommand::UpdateEvent { event: Box::new(event) }));
        }
        None => {
            event.calendar_id = default_calendar;
            route_calendar_save(calendar_state, event.calendar_id.clone(), event);
        }
    }
}

/// The organizer cancelled an event (iMIP `METHOD:CANCEL`): removes the
/// stored calendar object when one exists under the same UID, otherwise just
/// acknowledges. The banner was already dismissed by the caller.
fn remove_cancelled_imip_event(calendar_state: &Rc<RefCell<CalendarUiState>>, toast_overlay: &adw::ToastOverlay, invitation: &lookout_core::ImipInvitation) {
    let Some(mut event) = lookout_dav::parse_vevents(&CalendarId("iMIP".to_string()), &invitation.ics).into_iter().next() else {
        toast_overlay.add_toast(adw::Toast::new("Couldn't parse this cancellation's calendar data."));
        return;
    };
    match find_calendar_occurrence(calendar_state, &event.uid) {
        Some(existing) => {
            event.calendar_id = existing.calendar_id.clone();
            event.href = existing.href;
            event.etag = existing.etag;
            if let Some(handle) = calendar_handle_for_id(calendar_state, &event.calendar_id) {
                if let Some(href) = event.href.clone() {
                    let _ = handle.send_blocking(CalendarCommand::DeleteEvent {
                        calendar_id: event.calendar_id.clone(),
                        href,
                        etag: event.etag.clone(),
                    });
                    toast_overlay.add_toast(adw::Toast::new("Event removed from calendar"));
                    return;
                }
            }
            toast_overlay.add_toast(adw::Toast::new("Couldn't remove the event from your calendar."));
        }
        None => {
            toast_overlay.add_toast(adw::Toast::new("This event wasn't in your calendar."));
        }
    }
}

/// The stored occurrence matching `uid` inside `calendar_id` specifically.
/// (The iMIP upsert searches every calendar; an `.ics` import is scoped to
/// its target instead, since re-importing a file must update what the file
/// previously added, not touch a coincidental same-UID event elsewhere.)
fn find_occurrence_in_calendar(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_id: &CalendarId, uid: &EventUid) -> Option<EventOccurrence> {
    calendar_state
        .borrow()
        .accounts
        .values()
        .flat_map(|handle| handle.last_occurrences.iter())
        .find(|occurrence| occurrence.uid == *uid && occurrence.calendar_id == *calendar_id)
        .cloned()
}

/// The "Add calendar" dialog - the sidebar button's one-stop entry point for
/// the whole subscription/import story:
///
/// * **Subscribe** - a webcal/`https://…ics` feed URL (+ optional name) is
///   validated, persisted to `settings.json`, and handed to the webcal
///   session as a full `Reload` list (the session never learns of partial
///   changes - the list here is always authoritative).
/// * **Import** - a picked `.ics` file's events are routed into a chosen
///   CalDAV calendar through the existing session write path, upserting by
///   UID so re-importing the same file updates rather than duplicates.
/// * **Manage** - the current subscriptions, each with a Remove action, plus
///   a manual "Refresh now" for feeds that shouldn't wait for the poll.
///
/// Subscriptions are fetch-only, so import targets only ever list CalDAV
/// calendars (`pickable_calendars`), and removing a subscription also drops
/// its feed cache, its checked flag, and its colour assignment.
#[allow(clippy::too_many_arguments)]
/// The manage-list rebuild slot used by [`show_calendar_dialog`]: a closure
/// `Option` behind a `RefCell`, since the rebuild closure and the remove
/// handler are mutually referential (the slot breaks the cycle).
type ManageRebuild = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

#[allow(clippy::too_many_arguments)]
fn show_calendar_dialog(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    calendar_main: &Rc<CalendarMain>,
    calendar_list_box: &gtk::Box,
    mini_calendar: &calendar_view::MiniCalendar,
    mail_overview_day: &Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: &gtk::Box,
    toast_overlay: &adw::ToastOverlay,
) {
    let app_config = state.borrow().app_config.clone();

    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Add calendar")
        .default_width(460)
        .default_height(560)
        .build();

    // --- Subscribe section: URL (+ optional name) -> validated, persisted,
    // handed to the session.
    let url_entry = adw::EntryRow::builder().title("Feed URL").build();
    let name_entry = adw::EntryRow::builder().title("Name").build();
    let subscribe_button = gtk::Button::with_label("Subscribe");
    subscribe_button.add_css_class("suggested-action");
    subscribe_button.set_halign(gtk::Align::End);
    let subscribe_group = adw::PreferencesGroup::builder().title("Subscribe to a calendar").build();
    subscribe_group.add(&url_entry);
    subscribe_group.add(&name_entry);
    subscribe_group.add(&subscribe_button);

    // --- Import section: chosen .ics file -> parsed -> routed into the
    // picked CalDAV calendar (upsert by UID).
    let import_file_button = gtk::Button::with_label("Choose .ics file…");
    import_file_button.set_halign(gtk::Align::Start);
    let import_file_label = gtk::Label::builder()
        .label("No file chosen")
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    let chosen_ics: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let pickable = pickable_calendars(calendar_state);
    let import_calendar_labels: Vec<String> = pickable.iter().map(|(label, _)| label.clone()).collect();
    let import_calendar_ids: Vec<CalendarId> = pickable.iter().map(|(_, id)| id.clone()).collect();
    let import_target_dropdown = gtk::DropDown::builder().build();
    if import_calendar_ids.is_empty() {
        import_target_dropdown.set_sensitive(false);
    } else {
        let label_refs: Vec<&str> = import_calendar_labels.iter().map(String::as_str).collect();
        import_target_dropdown.set_model(Some(&gtk::StringList::new(&label_refs)));
        import_target_dropdown.set_selected(0);
    }
    import_target_dropdown.set_tooltip_text(Some("Import into this calendar"));
    let import_button = gtk::Button::with_label("Import");
    import_button.add_css_class("suggested-action");
    import_button.set_sensitive(false);
    import_button.set_halign(gtk::Align::End);
    let import_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    import_row.append(&import_file_button);
    import_row.append(&import_file_label);
    let import_row2 = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    import_row2.append(&import_target_dropdown);
    import_row2.append(&import_button);
    let import_group = adw::PreferencesGroup::builder().title("Import from file").build();
    import_group.add(&import_row);
    import_group.add(&import_row2);

    // --- Manage section: one row per subscription (name + Remove), and a
    // manual refresh for feeds that shouldn't wait for the 5-minute poll.
    let manage_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let refresh_now_button = gtk::Button::with_label("Refresh now");
    refresh_now_button.add_css_class("flat");
    refresh_now_button.set_halign(gtk::Align::Start);
    let manage_group = adw::PreferencesGroup::builder().title("Manage subscriptions").build();
    manage_group.add(&manage_box);
    manage_group.add(&refresh_now_button);

    // After any change: repaint the checklist/view/mini-calendar from the
    // current state (the session's own `SubscriptionsUpdated` also lands,
    // but this makes the change feel immediate - the removed handle is
    // already gone from state, so its events vanish right away).
    let refresh_ui: Rc<dyn Fn()> = Rc::new({
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let calendar_list_box = calendar_list_box.clone();
        let mini_calendar = mini_calendar.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        move || {
            refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
            refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
            let month = calendar_state.borrow().displayed_month;
            let event_days = calendar_event_days(&calendar_state, month);
            calendar_view::set_mini_event_days(&mini_calendar, &event_days);
        }
    });

    // Pushes the authoritative subscription list (from `settings.json`) to
    // the webcal session, which adopts it and re-syncs every feed.
    let reload_session: Rc<dyn Fn()> = Rc::new({
        let calendar_state = calendar_state.clone();
        let app_config = app_config.clone();
        move || {
            let subscriptions = app_config.borrow().webcal_subscriptions.clone();
            let cmd = calendar_state.borrow().webcal_cmd_tx.clone();
            if let Some(cmd) = cmd {
                let _ = cmd.send_blocking(SubscriptionCommand::Reload { subscriptions });
            }
        }
    });

    // Re-renders the manage list from current state (called at open and
    // after every subscribe/remove). The two closures are mutually
    // referential - `rebuild_manage` creates rows whose Remove buttons call
    // `remove_subscription`, which re-renders via `rebuild_manage` - so the
    // rebuild is stashed in a slot the remove handler fills in afterwards.
    let rebuild_manage: ManageRebuild = Rc::new(RefCell::new(None));
    let remove_subscription: Rc<dyn Fn(String)> = Rc::new({
        let app_config = app_config.clone();
        let calendar_state = calendar_state.clone();
        let reload_session = reload_session.clone();
        let refresh_ui = refresh_ui.clone();
        let rebuild_manage = rebuild_manage.clone();
        let toast_overlay = toast_overlay.clone();
        move |id: String| {
            {
                let mut config = app_config.borrow_mut();
                config.webcal_subscriptions.retain(|s| s.id != id);
                crate::app_config::save(&config);
            }
            {
                let mut st = calendar_state.borrow_mut();
                st.webcal_subscriptions.retain(|s| s.id != id);
                st.webcal_handles.remove(&id);
                st.checked_calendar_ids.remove(&webcal_calendar_id(&id));
                st.calendar_colors.remove(&webcal_calendar_id(&id));
            }
            calendar_colors::save(&calendar_state.borrow().calendar_colors);
            let _ = lookout_dav::remove_subscription_cache(&id);
            reload_session();
            refresh_ui();
            if let Some(rebuild) = rebuild_manage.borrow().clone() {
                rebuild();
            }
            toast_overlay.add_toast(adw::Toast::new("Subscription removed"));
        }
    });
    *rebuild_manage.borrow_mut() = Some(Rc::new({
        let manage_box = manage_box.clone();
        let calendar_state = calendar_state.clone();
        let remove_subscription = remove_subscription.clone();
        move || {
            while let Some(child) = manage_box.first_child() {
                manage_box.remove(&child);
            }
            let subs = calendar_state.borrow().webcal_subscriptions.clone();
            if subs.is_empty() {
                let label = gtk::Label::builder()
                    .label("No subscriptions yet")
                    .css_classes(["dim-label", "caption"])
                    .xalign(0.0)
                    .build();
                manage_box.append(&label);
            }
            for sub in subs {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let text = gtk::Label::builder()
                    .label(&sub.display_name)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .hexpand(true)
                    .xalign(0.0)
                    .build();
                text.set_tooltip_text(Some(&sub.url));
                let remove = gtk::Button::from_icon_name(themed_icon_name(&["user-trash-symbolic", "edit-delete-symbolic"]));
                remove.add_css_class("flat");
                remove.set_tooltip_text(Some("Remove subscription"));
                let id = sub.id.clone();
                let remove_subscription = remove_subscription.clone();
                remove.connect_clicked(move |_| remove_subscription(id.clone()));
                row.append(&text);
                row.append(&remove);
                manage_box.append(&row);
            }
        }
    }));

    subscribe_button.connect_clicked({
        let url_entry = url_entry.clone();
        let name_entry = name_entry.clone();
        let app_config = app_config.clone();
        let calendar_state = calendar_state.clone();
        let reload_session = reload_session.clone();
        let refresh_ui = refresh_ui.clone();
        let rebuild_manage = rebuild_manage.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let url_text = url_entry.text().trim().to_string();
            let Ok(url) = lookout_dav::normalize_webcal_url(&url_text) else {
                toast_overlay.add_toast(adw::Toast::new("That doesn't look like a calendar feed URL."));
                return;
            };
            let name = {
                let typed = name_entry.text().trim().to_string();
                if typed.is_empty() {
                    url.host_str().unwrap_or(&url_text).to_string()
                } else {
                    typed
                }
            };
            let subscription = WebcalSubscription {
                id: uuid::Uuid::new_v4().to_string(),
                display_name: name,
                url: url_text,
            };
            {
                let mut config = app_config.borrow_mut();
                config.webcal_subscriptions.push(subscription.clone());
                crate::app_config::save(&config);
            }
            {
                let mut st = calendar_state.borrow_mut();
                st.webcal_subscriptions.push(subscription.clone());
                st.webcal_handles.insert(
                    subscription.id.clone(),
                    WebcalHandle {
                        calendar_id: webcal_calendar_id(&subscription.id),
                        display_name: subscription.display_name.clone(),
                        last_occurrences: Vec::new(),
                        last_synced_month: None,
                        occurrences_by_month: HashMap::new(),
                        error: None,
                    },
                );
            }
            reload_session();
            refresh_ui();
            if let Some(rebuild) = rebuild_manage.borrow().clone() {
                rebuild();
            }
            url_entry.set_text("");
            name_entry.set_text("");
            toast_overlay.add_toast(adw::Toast::new("Subscribed - fetching events…"));
        }
    });

    import_file_button.connect_clicked({
        let window = window.clone();
        let import_file_label = import_file_label.clone();
        let chosen_ics = chosen_ics.clone();
        let import_button = import_button.clone();
        move |_| {
            let window = window.clone();
            let import_file_label = import_file_label.clone();
            let chosen_ics = chosen_ics.clone();
            let import_button = import_button.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("ics");
                filter.set_name(Some("iCalendar files (*.ics)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let dialog = gtk::FileDialog::builder().title("Choose a calendar file").filters(&filters).build();
                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                let Ok(raw) = std::fs::read(&path) else { return };
                *chosen_ics.borrow_mut() = Some(String::from_utf8_lossy(&raw).into_owned());
                import_file_label.set_label(&path.display().to_string());
                import_button.set_sensitive(true);
            });
        }
    });

    import_button.connect_clicked({
        let dialog = dialog.clone();
        let chosen_ics = chosen_ics.clone();
        let import_target_dropdown = import_target_dropdown.clone();
        let import_calendar_ids = import_calendar_ids.clone();
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(ics) = chosen_ics.borrow().clone() else { return };
            let Some(calendar_id) = import_calendar_ids.get(import_target_dropdown.selected() as usize).cloned() else {
                toast_overlay.add_toast(adw::Toast::new("Choose a calendar to import into."));
                return;
            };
            let events = lookout_dav::parse_vevents(&CalendarId("import".to_string()), &ics);
            if events.is_empty() {
                toast_overlay.add_toast(adw::Toast::new("No events found in this file."));
                return;
            }
            let mut created = 0;
            let mut updated = 0;
            for mut event in events {
                event.calendar_id = calendar_id.clone();
                match find_occurrence_in_calendar(&calendar_state, &calendar_id, &event.uid) {
                    Some(existing) => {
                        event.href = existing.href;
                        event.etag = existing.etag;
                        updated += 1;
                    }
                    None => {
                        event.href = None;
                        event.etag = None;
                        created += 1;
                    }
                }
                route_calendar_save(&calendar_state, calendar_id.clone(), event);
            }
            let summary = if updated > 0 {
                format!("Imported {created} event(s), updated {updated} existing")
            } else {
                format!("Imported {created} event(s)")
            };
            toast_overlay.add_toast(adw::Toast::new(&summary));
            dialog.close();
        }
    });

    refresh_now_button.connect_clicked({
        let calendar_state = calendar_state.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let cmd = calendar_state.borrow().webcal_cmd_tx.clone();
            if let Some(cmd) = cmd {
                let _ = cmd.send_blocking(SubscriptionCommand::Refresh);
                toast_overlay.add_toast(adw::Toast::new("Refreshing calendar feeds…"));
            }
        }
    });

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&subscribe_group);
    content.append(&import_group);
    content.append(&manage_group);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    dialog.set_content(Some(&scroller));
    if let Some(rebuild) = rebuild_manage.borrow().clone() {
        rebuild();
    }
    dialog.present();
}

fn mailbox_account_id(mailbox: &MailboxId) -> Option<AccountId> {
    mailbox.0.split_once(':').map(|(account_id, _)| AccountId(account_id.to_string()))
}

/// Which summaries a message-row drag carries: the whole current selection
/// when the dragged row is part of one, otherwise just that row's message.
fn dragged_summaries(message_list: &MessageListModel, bound: &Rc<RefCell<Option<EmailSummary>>>) -> Vec<EmailSummary> {
    let Some(row_summary) = bound.borrow().clone() else {
        return Vec::new();
    };
    let selected = message_list.selected_summaries();
    if selected.len() > 1 && selected.iter().any(|s| s.mailbox == row_summary.mailbox && s.uid == row_summary.uid) {
        selected
    } else {
        vec![row_summary]
    }
}

/// Writes the dragged messages' raw bytes - when already in the flat-file
/// cache - to temp `.eml` files and returns a `text/uri-list` content
/// provider for them, so the drag can land in a file manager. `None` when
/// none of the messages are cached (the internal move payload still works).
/// The written paths are recorded in `temp_files` for cleanup on drag end.
fn write_external_drag_files(state: &Rc<RefCell<UiState>>, summaries: &[EmailSummary], temp_files: &Rc<RefCell<Vec<PathBuf>>>) -> Option<gtk::gdk::ContentProvider> {
    let state = state.borrow();
    let mut files: Vec<gio::File> = Vec::new();
    for summary in summaries {
        let Some(account_id) = mailbox_account_id(&summary.mailbox) else { continue };
        let Some(handle) = state.accounts.get(&account_id) else { continue };
        let Some(cache) = handle.address_cache.as_ref() else { continue };
        let Some(uidvalidity) = handle.folders.iter().find(|f| f.id == summary.mailbox).map(|f| f.uidvalidity) else {
            continue;
        };
        let Ok(Some(bytes)) = cache.load_raw_message(&summary.mailbox, summary.uid, uidvalidity) else {
            continue;
        };
        let path = std::env::temp_dir().join(format!("lookout-drag-{}.eml", uuid::Uuid::new_v4()));
        if std::fs::write(&path, bytes).is_err() {
            continue;
        }
        temp_files.borrow_mut().push(path.clone());
        files.push(gio::File::for_path(path));
    }
    if files.is_empty() {
        return None;
    }
    let file_list = gtk::gdk::FileList::from_array(&files);
    Some(gtk::gdk::ContentProvider::for_value(&file_list.to_value()))
}

/// Handles a folder row's drop: deserializes the dragged `(mailbox, uid)`
/// list (the payload the message rows' drag source publishes as a
/// `G_TYPE_BYTES` value - a JSON list, so a drop from any other source fails
/// the parse and reads as "not ours" rather than a bogus move), keeps only
/// the messages belonging to the target folder's own account (messages can't
/// move across accounts), and issues one `MoveMessagesTo` per source mailbox.
/// `false` when the payload isn't ours (or empty), which lets GTK try other
/// drop targets.
fn handle_message_drag_drop(state: &Rc<RefCell<UiState>>, target: &Mailbox, value: &glib::Value) -> bool {
    let Ok(bytes) = value.get::<glib::Bytes>() else {
        return false;
    };
    let payload: Vec<(String, u32)> = match serde_json::from_slice(bytes.as_ref()) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("failed to parse message drag payload: {e}");
            return false;
        }
    };
    if payload.is_empty() {
        return false;
    }
    let mut groups: HashMap<AccountId, (MailboxId, Vec<Uid>)> = HashMap::new();
    for (mailbox, uid) in payload {
        let Some(account_id) = mailbox_account_id(&MailboxId(mailbox.clone())) else { continue };
        if account_id != target.account_id {
            continue;
        }
        let entry = groups.entry(account_id).or_insert_with(|| (MailboxId(mailbox), Vec::new()));
        entry.1.push(Uid(uid));
    }
    if groups.is_empty() {
        return false;
    }
    let state = state.borrow();
    for (account_id, (source, uids)) in groups {
        let Some(handle) = state.accounts.get(&account_id) else { continue };
        let _ = handle.cmd_tx.send_blocking(AccountCommand::MoveMessagesTo {
            mailbox: source,
            uids,
            target: target.id.clone(),
        });
    }
    true
}

/// The message-list header's mutable parts: the two title lines and the
/// favorite star. Bundled so the handful of places that change the active view
/// can refresh the header without each threading four widgets through its own
/// signature.
#[derive(Clone)]
struct ListHeader {
    folder_label: gtk::Label,
    account_label: gtk::Label,
    favorite_button: gtk::ToggleButton,
    /// Set while the star's state is being written programmatically, so the
    /// `toggled` handler can tell a refresh apart from a real user click.
    favorite_suppress: Rc<Cell<bool>>,
}

/// The header's two title lines for the active view: the open folder's display
/// name over its owning account. The unified view has no single mailbox behind
/// it, so it names itself and the whole account set.
fn current_view_title(state: &Rc<RefCell<UiState>>) -> (String, String) {
    let st = state.borrow();
    if matches!(st.mail_view, MailView::Search) {
        return ("Search results".to_string(), format!("“{}”", st.search_query));
    }
    if matches!(st.mail_view, MailView::UnifiedInbox) {
        return ("All Inboxes".to_string(), "All accounts".to_string());
    }
    let Some(mailbox_id) = st.current_mailbox.as_ref() else {
        return ("Inbox".to_string(), String::new());
    };
    let account_id = st.current_account.clone().or_else(|| mailbox_account_id(mailbox_id));
    let handle = account_id.as_ref().and_then(|id| st.accounts.get(id));
    // Folders can arrive after the selection does (a restored view names a
    // mailbox its account hasn't listed yet), so fall back to the id's path
    // segment rather than showing nothing.
    let folder = handle
        .and_then(|h| h.folders.iter().find(|m| &m.id == mailbox_id))
        .map(|m| display_name(&m.name))
        .or_else(|| mailbox_id.0.split_once(':').map(|(_, path)| display_name(path)))
        .unwrap_or_else(|| display_name(&mailbox_id.0));
    let account = handle
        .map(|h| if h.display_name.is_empty() { h.email.clone() } else { h.display_name.clone() })
        .unwrap_or_default();
    (folder, account)
}

/// Renders the header for whatever the message list is currently showing:
/// title lines, plus the star's pressed state and whether it's actionable at
/// all (the unified view spans every account, so there's no single folder to
/// favorite).
fn refresh_list_header(state: &Rc<RefCell<UiState>>, header: &ListHeader) {
    let (folder, account) = current_view_title(state);
    header.folder_label.set_label(&folder);
    header.account_label.set_label(&account);
    header.account_label.set_visible(!account.is_empty());

    let (favorable, starred) = {
        let st = state.borrow();
        // Search results span folders (and accounts), so the star - which
        // favorites the *open folder* - is meaningless while searching.
        match (st.mail_view, st.current_mailbox.as_ref()) {
            (MailView::Search, _) => (false, false),
            (_, Some(mailbox)) => (true, st.favorites.contains(mailbox)),
            (_, None) => (false, false),
        }
    };
    header.favorite_suppress.set(true);
    header.favorite_button.set_sensitive(favorable);
    header.favorite_button.set_active(starred);
    apply_favorite_visual(&header.favorite_button, starred);
    header.favorite_suppress.set(false);
}

/// Decodes a nav-rail SVG into a fixed-size `gtk::Image` for the view
/// buttons, which use full-colour artwork rather than theme icon names.
/// The bundled copy is preferred (see `resources.rs`); `fallback` covers
/// builds whose GResource bundle couldn't be compiled.
fn nav_rail_image(resource_path: &str, fallback: &'static [u8]) -> gtk::Image {
    let bytes = crate::resources::bytes(resource_path).unwrap_or_else(|| glib::Bytes::from_static(fallback));
    let texture = gtk::gdk::Texture::from_bytes(&bytes).expect("bundled nav-rail SVG should decode");
    let image = gtk::Image::from_paintable(Some(&texture));
    image.set_pixel_size(28);
    image
}

/// Keeps the favorite star's icon and tooltip in step with its pressed state.
fn apply_favorite_visual(button: &gtk::ToggleButton, starred: bool) {
    let icon = if starred {
        themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"])
    };
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(if starred { "Remove from Favorites" } else { "Add to Favorites" }));
}

/// The Star/Unstar button's icon for a given "is this starred" state -
/// filled when `starred`, outline otherwise. Shared by `refresh_star_button`
/// (recomputed from the message list's live data - selection change, and
/// after a server-confirmed `MessagesUpdated`) and the click handler's own
/// immediate flip (unlike Mark Read, starring isn't optimistically patched
/// into the message list model, so the button updates itself right away
/// rather than waiting on that confirmation).
fn star_icon_name(starred: bool) -> &'static str {
    if starred {
        themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"])
    }
}

/// Sets `star_button`'s icon to reflect whether the selected message(s) are
/// starred (`\Flagged`) - filled only when every selected message is
/// starred, outline otherwise (including nothing selected). Mirrors
/// `apply_favorite_visual`'s icon-swap convention.
fn refresh_star_button(button: &gtk::Button, message_list: &MessageListModel) {
    let summaries = message_list.selected_summaries();
    let starred = !summaries.is_empty() && summaries.iter().all(|s| s.is_starred());
    button.set_icon_name(star_icon_name(starred));
}

/// Sets `mark_read_button`'s icon/tooltip to reflect what clicking it would
/// currently do, from the message list's live selection - full-opacity
/// icons only (the stock `mail-read-symbolic` has `fill-opacity: 0.5` baked
/// into its own SVG, which reads as disabled next to this toolbar's other
/// solid icons): a checkmark when any selected message is unread (clicking
/// marks everything read), an envelope when the selection is already all
/// read (clicking marks everything unread) - matching the same aggregate
/// direction the click handler itself computes. Mirrors
/// `apply_favorite_visual`'s icon-swap convention.
fn refresh_mark_read_button(button: &gtk::Button, message_list: &MessageListModel) {
    let summaries = message_list.selected_summaries();
    let mark_read = summaries.is_empty() || summaries.iter().any(|s| s.is_unread());
    let (icon, tooltip) = if mark_read {
        (themed_icon_name(&["mail-mark-read-symbolic", "object-select-symbolic", "emblem-ok-symbolic"]), "Mark Read")
    } else {
        (
            themed_icon_name(&["mail-mark-unread-symbolic", "mail-unread-symbolic", "emblem-ok-symbolic"]),
            "Mark Unread",
        )
    };
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
}

/// The first of `candidates` the current icon theme actually has. Icon names
/// in this app resolve against the machine's theme rather than a bundled set
/// (see `folder_icon_name`), and the header's sort/filter/star icons are names
/// this codebase hasn't used before - so fall back to one that's already
/// proven in-tree instead of rendering a "missing image" box. The last
/// candidate is used unconditionally if none match.
pub(crate) fn themed_icon_name(candidates: &[&'static str]) -> &'static str {
    let theme = gtk::gdk::Display::default().map(|display| gtk::IconTheme::for_display(&display));
    if let Some(theme) = theme {
        for name in candidates {
            if theme.has_icon(name) {
                return name;
            }
        }
    }
    candidates.last().copied().unwrap_or("image-missing-symbolic")
}

/// (Re)loads the `.message-tag-dot.tag-<key>` color rules for the current tag
/// set into `provider`. `load_from_string` replaces the provider's previous
/// rules wholesale, so a rename/recolor/delete takes effect immediately and a
/// deleted tag's old class stops matching. Registered once for the display's
/// lifetime (see `build_window`); this is the only thing that writes it.
fn apply_tag_colors(tags: &Rc<RefCell<crate::tags::TagSet>>, provider: &gtk::CssProvider) {
    let mut css = String::new();
    for tag in &tags.borrow().tags {
        css.push_str(&format!(".message-tag-dot.tag-{} {{ background-color: {}; }}\n", tag.key, tag.color));
    }
    provider.load_from_string(&css);
}

/// Sends a `StoreKeywords` command for `summary` to its owning account's
/// session. The session `STORE`s the atoms, patches the cache, and re-emits,
/// which repaints the row's tag dots.
fn send_keyword_store(state: &Rc<RefCell<UiState>>, summary: &EmailSummary, add: Vec<String>, remove: Vec<String>) {
    let Some(account_id) = mailbox_account_id(&summary.mailbox) else { return };
    let st = state.borrow();
    let Some(handle) = st.accounts.get(&account_id) else { return };
    let _ = handle.cmd_tx.send_blocking(AccountCommand::StoreKeywords {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
        add,
        remove,
    });
}

/// Applies the tag `key` to every message in the dragged payload - the drop
/// side of the tag menu rows' drop targets. Groups the messages per
/// `(account, mailbox)` and issues one `StoreKeywordsMany` per group, the
/// same way a folder drop groups `MoveMessagesTo`. `false` when the payload
/// isn't a message drag (or empty), letting GTK try other drop targets.
fn handle_keyword_drag_drop(state: &Rc<RefCell<UiState>>, key: &str, value: &glib::Value) -> bool {
    let Ok(bytes) = value.get::<glib::Bytes>() else {
        return false;
    };
    let payload: Vec<(String, u32)> = match serde_json::from_slice(bytes.as_ref()) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("failed to parse message drag payload: {e}");
            return false;
        }
    };
    if payload.is_empty() {
        return false;
    }
    let keyword = lookout_core::tag_keyword(key);
    let state = state.borrow();
    let mut groups: HashMap<AccountId, (MailboxId, Vec<Uid>)> = HashMap::new();
    for (mailbox, uid) in payload {
        let Some(account_id) = mailbox_account_id(&MailboxId(mailbox.clone())) else { continue };
        let entry = groups.entry(account_id).or_insert_with(|| (MailboxId(mailbox), Vec::new()));
        entry.1.push(Uid(uid));
    }
    let mut sent = false;
    for (account_id, (source, uids)) in groups {
        let Some(handle) = state.accounts.get(&account_id) else { continue };
        let _ = handle.cmd_tx.send_blocking(AccountCommand::StoreKeywordsMany {
            mailbox: source,
            uids,
            add: vec![keyword.clone()],
            remove: Vec::new(),
        });
        sent = true;
    }
    sent
}

/// Builds the "More" menu popover's contents: "Save as .eml…" exports the
/// selected message's whole raw RFC 5322 bytes to a file, and "Print…" sends
/// the visible body through WebKit's print pipeline. Rebuilt on every popover
/// `show` (like the categorize menu) so each item's sensitivity tracks
/// whether a message is selected when the menu opens.
fn build_more_menu(has_selection: bool, message_list: &MessageListModel, state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, popover: &gtk::Popover) -> gtk::Box {
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let save_eml = gtk::Button::builder().label("Save as .eml…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    save_eml.set_sensitive(has_selection);
    {
        let message_list = message_list.clone();
        let state = state.clone();
        let popover = popover.clone();
        save_eml.connect_clicked(move |_| {
            popover.popdown();
            start_raw_message_export(&message_list, &state);
        });
    }
    boxed.append(&save_eml);
    let print_button = gtk::Button::builder().label("Print…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    // Print is only meaningful while a message is actually showing in the
    // reading pane; the selection alone isn't enough (it may be a selection
    // the pane hasn't rendered yet, or nothing rendered at all).
    print_button.set_sensitive(has_selection && reading_stack.visible_child_name().as_deref() == Some("message"));
    {
        let popover = popover.clone();
        let reading_stack = reading_stack.clone();
        print_button.connect_clicked(move |_| {
            popover.popdown();
            let Some(parent) = popover.root().and_downcast::<gtk::Window>() else { return };
            print_visible_message(&reading_stack, &parent);
        });
    }
    boxed.append(&print_button);
    boxed
}

/// Prints the currently-visible message body. HTML bodies print directly
/// through the reading pane's WebView; plain-text bodies (rendered in the
/// `GtkTextView` fallback) are wrapped in a minimal HTML page and printed
/// through an offscreen WebView, so every message goes through WebKit's
/// print pipeline.
fn print_visible_message(reading_stack: &gtk::Stack, parent: &gtk::Window) {
    let Some(content_stack) = find_named_child(reading_stack, "body").and_then(|child| child.downcast::<gtk::Stack>().ok()) else {
        return;
    };
    match content_stack.visible_child_name().as_deref() {
        Some("html") => {
            if let Some(web_view) = content_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
                let _ = webkit::PrintOperation::new(&web_view).run_dialog(Some(parent));
            }
        }
        Some("text") => {
            let text = content_stack
                .child_by_name("text")
                .and_downcast::<gtk::ScrolledWindow>()
                .and_then(|scroller| scroller.child())
                .and_downcast::<gtk::TextView>()
                .map(|text_view| text_view.buffer().text(&text_view.buffer().start_iter(), &text_view.buffer().end_iter(), false))
                .unwrap_or_default();
            let html = format!(
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
                 <style>body {{ font-family: sans-serif; white-space: pre-wrap; margin: 1em; }}</style>\
                 </head><body>{}</body></html>",
                glib::markup_escape_text(&text)
            );
            print_html_once(&html, parent);
        }
        _ => {}
    }
}

/// Prints an HTML string through a throwaway offscreen WebView once its load
/// finishes; the WebKit print dialog is modal, so the page only needs to be
/// parsed far enough to lay out - it never appears on screen.
pub(crate) fn print_html_once<T: IsA<gtk::Window>>(html: &str, parent: &T) {
    let parent = parent.clone();
    let web_view = webkit::WebView::builder().build();
    web_view.connect_load_changed(move |web_view, event| {
        if event == webkit::LoadEvent::Finished {
            let _ = webkit::PrintOperation::new(web_view).run_dialog(Some(&parent));
        }
    });
    web_view.load_html(html, None);
}

/// Builds the tag menu shown by both the toolbar Categorize popover and the
/// message row's right-click menu: one toggle row per defined tag (a color
/// dot plus a check button, checked when `target` carries that tag's
/// keyword), then a "Manage tags…" row. `target` is the message the toggles
/// act on; `None` (nothing selected) renders the toggles disabled. The
/// caller owns showing it - for the toolbar it's the popover child (rebuilt
/// on every `show`), for a row it's the row's context popover.
fn build_tag_menu(
    tags: &Rc<RefCell<crate::tags::TagSet>>,
    state: &Rc<RefCell<UiState>>,
    target: Option<EmailSummary>,
    message_list: &MessageListModel,
    tag_colors: &gtk::CssProvider,
) -> gtk::Box {
    let set = tags.borrow();
    let boxed = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    if set.tags.is_empty() {
        let empty = gtk::Label::builder()
            .label("No tags defined yet")
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .margin_top(2)
            .margin_bottom(2)
            .build();
        boxed.append(&empty);
    } else {
        for tag in &set.tags {
            let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
            let dot = gtk::Box::builder()
                .width_request(10)
                .height_request(10)
                .css_classes(["message-tag-dot", &format!("tag-{}", tag.key)])
                .valign(gtk::Align::Center)
                .build();
            let toggle = gtk::CheckButton::builder().label(&tag.name).build();
            let has = target.as_ref().is_some_and(|s| s.keywords.contains(&lookout_core::tag_keyword(&tag.key)));
            toggle.set_active(has);
            toggle.set_sensitive(target.is_some());
            row.append(&dot);
            row.append(&toggle);

            let summary = target.clone();
            let key = tag.key.clone();
            let state = state.clone();
            let state_for_toggle = state.clone();
            toggle.connect_toggled(move |t| {
                if let Some(summary) = &summary {
                    let keyword = lookout_core::tag_keyword(&key);
                    let (add, remove) = if t.is_active() { (vec![keyword], Vec::new()) } else { (Vec::new(), vec![keyword]) };
                    send_keyword_store(&state_for_toggle, summary, add, remove);
                }
            });
            // Dropping messages onto the tag row applies that tag to all of
            // them - the batch counterpart of the toggles (one
            // `StoreKeywordsMany` per source mailbox).
            {
                let drop_state = state.clone();
                let drop_key = tag.key.clone();
                let row_for_enter = row.clone();
                let row_for_leave = row.clone();
                let drop_target = gtk::DropTarget::builder()
                    .formats(&gtk::gdk::ContentFormats::for_type(glib::Bytes::static_type()))
                    .actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE)
                    .build();
                drop_target.connect_drop(move |_target, value, _x, _y| handle_keyword_drag_drop(&drop_state, &drop_key, value));
                drop_target.connect_enter(move |_target, _x, _y| {
                    row_for_enter.add_css_class("lookout-drop-target");
                    gtk::gdk::DragAction::COPY
                });
                drop_target.connect_leave(move |_target| {
                    row_for_leave.remove_css_class("lookout-drop-target");
                });
                row.add_controller(drop_target);
            }
            boxed.append(&row);
        }
    }

    let manage = gtk::Button::builder().label("Manage tags…").css_classes(["flat"]).halign(gtk::Align::Start).build();
    {
        let tags = tags.clone();
        let message_list = message_list.clone();
        let tag_colors = tag_colors.clone();
        let manage_button = manage.clone();
        manage.connect_clicked(move |_| show_manage_tags_dialog(manage_button.upcast_ref::<gtk::Widget>(), tags.clone(), message_list.clone(), tag_colors.clone()));
    }
    boxed.append(&manage);
    boxed
}

/// The tag-management dialog: list existing tags (color swatch + editable
/// name + delete), with an "add" row for new ones. Mutates the shared
/// `TagSet`, persists it via `crate::tags::save`, refreshes the row color
/// rules (`apply_tag_colors`), and forces the message list to re-render -
/// a recolor/rename doesn't change any message's keywords, so the list's
/// no-op check would otherwise skip the rebuild.
///
/// Deleting a tag is non-destructive: only the definition is removed. The
/// `$Lookout-tag-*` keywords already stored on messages stay on the server
/// and simply stop displaying.
fn show_manage_tags_dialog(anchor: &gtk::Widget, tags: Rc<RefCell<crate::tags::TagSet>>, message_list: MessageListModel, tag_colors: gtk::CssProvider) {
    let window = anchor.root().and_downcast::<gtk::Window>();
    let dialog = {
        let mut builder = gtk::Window::builder().modal(true).title("Manage tags").default_width(440).default_height(520);
        if let Some(win) = window {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
    let scroller = gtk::ScrolledWindow::builder().child(&list).vexpand(true).build();

    let color_dialog = gtk::ColorDialog::new();
    // `rebuild` re-renders the tag list; its per-row handlers call it again
    // after an edit. An `Rc<RefCell<Box<dyn Fn()>>>` so a handler created
    // while it runs can still reach it. (The handlers clone the `Rc`, which
    // forms a reference cycle that lives until the dialog's widgets drop -
    // a bounded, one-off cost for a modal dialog.)
    let rebuild: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    let rebuild_handle = rebuild.clone();
    {
        let list = list.clone();
        let tags_rc = tags.clone();
        let tag_colors_rc = tag_colors.clone();
        let color_dialog = color_dialog.clone();
        *rebuild.borrow_mut() = Box::new(move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let set = tags_rc.borrow();
            if set.tags.is_empty() {
                let empty = gtk::Label::builder()
                    .label("No tags yet - add one below.")
                    .halign(gtk::Align::Start)
                    .css_classes(["dim-label"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .build();
                list.append(&empty);
            }
            for tag in &set.tags {
                let row = gtk::ListBoxRow::new();
                let row_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(10)
                    .margin_top(6)
                    .margin_bottom(6)
                    .margin_start(10)
                    .margin_end(10)
                    .build();

                let swatch = gtk::ColorDialogButton::builder().dialog(&color_dialog).build();
                swatch.set_rgba(&hex_to_rgba(&tag.color));
                swatch.set_tooltip_text(Some("Change color"));

                let name = gtk::Entry::builder().text(&tag.name).build();

                let delete = gtk::Button::from_icon_name("user-trash-symbolic");
                delete.set_tooltip_text(Some("Delete tag"));
                delete.add_css_class("flat");

                row_box.append(&swatch);
                row_box.append(&name);
                row_box.append(&delete);
                row.set_child(Some(&row_box));
                list.append(&row);

                let key = tag.key.clone();
                let tags = tags_rc.clone();
                let tag_colors = tag_colors_rc.clone();
                let rebuild = rebuild_handle.clone();
                swatch.connect_rgba_notify(move |swatch| {
                    let mut set = tags.borrow_mut();
                    if let Some(tag) = set.tags.iter_mut().find(|t| t.key == key) {
                        tag.color = rgba_to_hex(&swatch.rgba());
                    }
                    drop(set);
                    crate::tags::save(&tags.borrow());
                    apply_tag_colors(&tags, &tag_colors);
                    (rebuild.borrow())();
                });

                let key = tag.key.clone();
                let tags = tags_rc.clone();
                name.connect_changed(move |entry| {
                    let mut set = tags.borrow_mut();
                    if let Some(tag) = set.tags.iter_mut().find(|t| t.key == key) {
                        tag.name = entry.text().trim().to_string();
                    }
                    drop(set);
                    crate::tags::save(&tags.borrow());
                });

                let key = tag.key.clone();
                let tags = tags_rc.clone();
                let tag_colors = tag_colors_rc.clone();
                let rebuild = rebuild_handle.clone();
                delete.connect_clicked(move |_| {
                    tags.borrow_mut().tags.retain(|t| t.key != key);
                    crate::tags::save(&tags.borrow());
                    apply_tag_colors(&tags, &tag_colors);
                    (rebuild.borrow())();
                });
            }
        });
    }
    (rebuild.borrow())();

    // --- Add-a-tag row: name entry + color swatch + Add button ---
    let add_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(8)
        .margin_start(10)
        .margin_end(10)
        .build();
    let new_name = gtk::Entry::builder().placeholder_text("New tag name").hexpand(true).build();
    let new_color = gtk::ColorDialogButton::builder().dialog(&color_dialog).build();
    new_color.set_rgba(&hex_to_rgba(crate::tags::default_tag_color(tags.borrow().tags.len())));
    let add_button = gtk::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_row.append(&new_name);
    add_row.append(&new_color);
    add_row.append(&add_button);

    let error_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .css_classes(["error"])
        .visible(false)
        .margin_start(10)
        .margin_end(10)
        .build();
    {
        let tags = tags.clone();
        let new_name = new_name.clone();
        let new_color = new_color.clone();
        let error_label = error_label.clone();
        let tag_colors = tag_colors.clone();
        let rebuild = rebuild.clone();
        add_button.connect_clicked(move |_| {
            let key = lookout_core::sanitize_tag_key(new_name.text().trim());
            let name = new_name.text().trim().to_string();
            if name.is_empty() || key.is_empty() {
                error_label.set_label("Enter a tag name.");
                error_label.set_visible(true);
                return;
            }
            if tags.borrow().contains_key(&key) {
                error_label.set_label("A tag with this name already exists.");
                error_label.set_visible(true);
                return;
            }
            let color = rgba_to_hex(&new_color.rgba());
            tags.borrow_mut().tags.push(crate::tags::TagDef { key, name, color });
            crate::tags::save(&tags.borrow());
            apply_tag_colors(&tags, &tag_colors);
            error_label.set_visible(false);
            new_name.set_text("");
            new_color.set_rgba(&hex_to_rgba(crate::tags::default_tag_color(tags.borrow().tags.len())));
            (rebuild.borrow())();
        });
    }

    let close_button = gtk::Button::with_label("Close");
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    content.append(&scroller);
    content.append(&add_row);
    content.append(&error_label);
    {
        let dialog = dialog.clone();
        close_button.connect_clicked(move |_| dialog.close());
    }
    close_button.set_halign(gtk::Align::End);
    close_button.set_margin_end(12);
    content.append(&close_button);
    dialog.set_child(Some(&content));
    dialog.present();

    // Opening the dialog is itself a tag-definition view change that can
    // alter row colors if any were edited in an earlier session and then the
    // file changed under us; a refresh is cheap and always correct.
    message_list.refresh();
}

/// `#rrggbb` -> `gdk::RGBA`. Any malformed input degrades to a mid-grey
/// rather than failing, matching the "colors are cosmetic" spirit of
/// `calendar_colors::resolve_color`.
fn hex_to_rgba(color: &str) -> gtk::gdk::RGBA {
    let body = color.strip_prefix('#').unwrap_or(color);
    let byte = |i: usize| u8::from_str_radix(&body[i..i + 2], 16).unwrap_or(128);
    let (r, g, b) = match body.len() {
        6 if body.bytes().all(|c| c.is_ascii_hexdigit()) => (byte(0), byte(2), byte(4)),
        _ => (128, 128, 128),
    };
    gtk::gdk::RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

/// `gdk::RGBA` -> `#rrggbb` (alpha dropped - tags are opaque).
fn rgba_to_hex(rgba: &gtk::gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.red() * 255.0).round() as u8,
        (rgba.green() * 255.0).round() as u8,
        (rgba.blue() * 255.0).round() as u8
    )
}

/// The sort-direction toggle's icon for the current order.
fn sort_direction_icon(descending: bool) -> &'static str {
    if descending {
        themed_icon_name(&["view-sort-descending-symbolic", "go-down-symbolic"])
    } else {
        themed_icon_name(&["view-sort-ascending-symbolic", "go-up-symbolic"])
    }
}

/// Runs `query` against `cache` on the `Worker`'s blocking thread pool and
/// returns a receiver for the result, instead of running it inline on the
/// GTK thread. Mirrors the mail session's own `cache_op` helper for the
/// blocking-pool dispatch, and `contacts_view`'s `dispatch_contact_command`
/// for the bounded(1)-reply-channel shape that gets the value back without
/// blocking the main loop: the caller sends nothing and blocks on nothing,
/// it `.await`s `reply_rx.recv()` inside a `glib::spawn_future_local`.
///
/// A dropped reply (the query panicked, or the runtime is shutting down)
/// simply closes the channel - callers already treat a closed/empty receiver
/// as "no result," the same as a cache miss.
pub(crate) fn spawn_cache_read<T: Send + 'static>(
    worker: &Worker,
    cache: Arc<lookout_mail::Cache>,
    query: impl FnOnce(&lookout_mail::Cache) -> T + Send + 'static,
) -> async_channel::Receiver<T> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        if let Ok(result) = tokio::task::spawn_blocking(move || query(&cache)).await {
            let _ = reply_tx.send(result).await;
        }
    });
    reply_rx
}

/// Switches the message list to a single mailbox and asks its owning account
/// to sync it. Shared by the folder-selection handler and the account
/// switcher's fallback path.
fn select_mailbox(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    message_list: &MessageListModel,
    message_list_stack: &gtk::Stack,
    account_id: AccountId,
    mailbox_id: MailboxId,
) {
    {
        let mut st = state.borrow_mut();
        st.mail_view = MailView::Single;
        st.current_account = Some(account_id.clone());
        st.current_mailbox = Some(mailbox_id.clone());
        st.restore_pending = false;
        last_view::save(
            &st.settings,
            &LastSelection {
                unified: false,
                mailbox: Some(mailbox_id.0.clone()),
            },
        );
    }
    // Clear immediately rather than leaving the previously-selected folder's
    // rows on screen - the actual cached content for the new folder paints
    // in a moment later, once the off-thread read (below) answers. Mirrors
    // `exit_search`'s single-view repaint.
    let (key, descending) = current_sort(state);
    message_list.repopulate(Vec::new(), key, descending);
    request_mailbox_sync(state, &account_id, &mailbox_id);
    refresh_message_loading_state(state, message_list, message_list_stack);

    if let Some(cache) = state.borrow().accounts.get(&account_id).and_then(|h| h.address_cache.clone()) {
        let reply_rx = spawn_cache_read(worker, cache, {
            let mailbox_id = mailbox_id.clone();
            move |cache| {
                let mut messages = cache.load_messages(&mailbox_id).ok()?;
                let snoozed = cache.active_snoozed_uids(&mailbox_id, chrono::Utc::now()).unwrap_or_default();
                messages.retain(|m| !snoozed.contains(&m.uid));
                Some(messages)
            }
        });
        let state = state.clone();
        let message_list = message_list.clone();
        let message_list_stack = message_list_stack.clone();
        glib::spawn_future_local(async move {
            let Ok(Some(cached)) = reply_rx.recv().await else { return };
            // Discard a reply for a mailbox the user has since switched away
            // from - applying it now would repaint the wrong folder.
            if state.borrow().current_mailbox.as_ref() != Some(&mailbox_id) {
                return;
            }
            let (key, descending) = current_sort(&state);
            message_list.repopulate(cached, key, descending);
            // The list just painted from cache - clear the spinner now rather
            // than leaving it up until the separate live IMAP sync's
            // `MessagesUpdated` arrives. Without this, a cache hit still shows
            // "loading" for the full round trip to the account session and
            // back, even though the correct rows are already on screen
            // underneath it.
            refresh_message_loading_state(&state, &message_list, &message_list_stack);
        });
    }
}

/// Requests a mailbox sync from its account, deduplicating against
/// in-flight requests: a `SyncMailbox` that's already outstanding (sent but
/// not yet answered by a `MessagesUpdated`) suppresses a duplicate. Without
/// this the startup burst - the session's cache replay plus the app's
/// on-demand syncs for the same inbox - queues several identical syncs.
/// Returns whether a request was actually sent. Pending entries are cleared
/// by the `MessagesUpdated` that answers them and by a `FoldersUpdated`
/// (every reconnect re-lists folders), so a request that dies with a dropped
/// connection can never permanently suppress a later one.
fn request_mailbox_sync(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox_id: &MailboxId) -> bool {
    let mut st = state.borrow_mut();
    if st.syncing.contains(mailbox_id) {
        return false;
    }
    st.syncing.insert(mailbox_id.clone());
    if let Some(handle) = st.accounts.get(account_id) {
        let _ = handle.cmd_tx.send_blocking(AccountCommand::SyncMailbox(mailbox_id.clone()));
    }
    true
}

/// Shows the "Fetching message list." spinner in place of the message list
/// exactly while the view on screen is empty *and* its sync is still
/// outstanding (tracked in `UiState::syncing`) - never for a folder that's
/// genuinely empty, since nothing ever clears `syncing` for a mailbox that
/// isn't waiting on anything, and a permanently-spinning "fetching" message
/// would be worse than the plain empty list it replaces. Call after anything
/// that can change either input: a `repopulate`, or a `syncing`/
/// `FoldersUpdated` change.
fn refresh_message_loading_state(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, message_list_stack: &gtk::Stack) {
    let empty = message_list.selection.n_items() == 0;
    let waiting = if empty {
        let (mail_view, current_mailbox, syncing) = {
            let st = state.borrow();
            (st.mail_view, st.current_mailbox.clone(), st.syncing.clone())
        };
        match mail_view {
            MailView::Single => current_mailbox.is_some_and(|m| syncing.contains(&m)),
            MailView::UnifiedInbox => account_inboxes(state).into_iter().any(|(_, inbox)| syncing.contains(&inbox)),
            MailView::Search => false,
        }
    } else {
        false
    };
    message_list_stack.set_visible_child_name(if waiting { "loading" } else { "list" });
}

/// Every connected account's Inbox - the mailbox set the unified view is
/// built from.
fn account_inboxes(state: &Rc<RefCell<UiState>>) -> Vec<(AccountId, MailboxId)> {
    state
        .borrow()
        .accounts
        .iter()
        .filter_map(|(id, handle)| handle.folders.iter().find(|m| matches!(m.role, MailboxRole::Inbox)).map(|m| (id.clone(), m.id.clone())))
        .collect()
}

/// Re-syncs whatever the message list is currently showing: the open mailbox,
/// or every account's Inbox in the unified view. `request_mailbox_sync`
/// dedupes against in-flight requests, so pressing Sync while a sync is
/// already outstanding is deliberately a no-op rather than a second round
/// trip.
fn resync_current_view(state: &Rc<RefCell<UiState>>) {
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        for (account_id, inbox_id) in account_inboxes(state) {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
        return;
    }
    let current = {
        let st = state.borrow();
        st.current_account.clone().zip(st.current_mailbox.clone())
    };
    if let Some((account_id, mailbox_id)) = current {
        request_mailbox_sync(state, &account_id, &mailbox_id);
    }
}

/// The FTS cache pass of a search: query every connected account's read-side
/// cache handle and merge the hits, deduplicated by `(mailbox, uid)` so a
/// message that matches on both subject and body still appears once. Runs on
/// the UI thread against the on-disk index (the same read-side handles the
/// composer's autocomplete uses) - no IMAP round trip, so this is what makes
/// results feel instant.
/// Fires one off-thread FTS query per connected account's cache and returns
/// a receiver for each - the caller joins them (see `start_search`) rather
/// than this function blocking the GTK thread until every account answers.
fn search_cached_results(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, query: &str) -> Vec<async_channel::Receiver<Vec<EmailSummary>>> {
    state
        .borrow()
        .accounts
        .values()
        .filter_map(|h| h.address_cache.clone())
        .map(|cache| {
            let query = query.to_string();
            spawn_cache_read(worker, cache, move |cache| cache.search(&query, SEARCH_RESULT_LIMIT).unwrap_or_default())
        })
        .collect()
}

/// Repopulates the message list from the accumulated search results, under the
/// current sort.
fn repopulate_search_results(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel) {
    let (key, descending) = current_sort(state);
    let results = state.borrow().search_results.clone();
    message_list.repopulate(results, key, descending);
}

/// Asks the live IMAP pass to cover the mailbox the user was viewing - or
/// every account's Inbox when the pre-search view was the unified one (the
/// same folders `account_inboxes` names). Searches arrive in these folders, so
/// this is where the local index's gaps are: mail that has never been synced,
/// and body text for messages never fetched. One `SearchMailbox` per folder
/// (a SELECT + `UID SEARCH` + fetch each), which is why the fan-out stops at
/// the open view instead of covering every folder of every account - a search
/// over an unopened folder would otherwise pay a round trip for nothing the
/// user is looking at.
fn dispatch_search_fallbacks(state: &Rc<RefCell<UiState>>, query: &str) {
    let targets: Vec<(AccountId, MailboxId)> = {
        let st = state.borrow();
        if st.current_mailbox.is_some() {
            st.current_account.clone().zip(st.current_mailbox.clone()).into_iter().collect()
        } else {
            account_inboxes(state)
        }
    };
    let mut sent: Vec<(AccountId, MailboxId)> = Vec::new();
    {
        let mut st = state.borrow_mut();
        for (account_id, mailbox) in targets {
            if st.search_pending.insert((account_id.clone(), mailbox.clone())) {
                sent.push((account_id, mailbox));
            }
        }
    }
    for (account_id, mailbox) in sent {
        if let Some(handle) = state.borrow().accounts.get(&account_id) {
            let _ = handle.cmd_tx.send_blocking(AccountCommand::SearchMailbox {
                mailbox,
                query: query.to_string(),
            });
        }
    }
}

/// Enters (or re-enters) full-text search for `query`: flips the list into
/// search mode, dispatches the live IMAP pass on the open view, and repopulates
/// from the local FTS index across every account as each one's off-thread
/// query answers (an empty list paints immediately in the meantime). An empty
/// query is `exit_search` by another name (clearing the entry's X ends the
/// search).
fn start_search(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, message_list: &MessageListModel, list_header: &ListHeader, query: &str) {
    let query = query.trim().to_string();
    debug_assert!(!query.is_empty(), "start_search is only called with a non-empty query; empty text takes the exit path");
    {
        let mut st = state.borrow_mut();
        st.search_active = true;
        st.search_query = query.clone();
        st.mail_view = MailView::Search;
        // Drop any folders the *previous* query's live pass was waiting on.
        // An answer arriving for them is then both un-pending and (via the
        // `query != search_query` check in the event loop) for the wrong
        // query, so it's discarded either way.
        st.search_pending.clear();
        st.search_results.clear();
    }
    dispatch_search_fallbacks(state, &query);
    repopulate_search_results(state, message_list);
    refresh_list_header(state, list_header);

    let receivers = search_cached_results(state, worker, &query);
    let state = state.clone();
    let message_list = message_list.clone();
    glib::spawn_future_local(async move {
        let mut seen: HashSet<(MailboxId, Uid)> = HashSet::new();
        let mut results = Vec::new();
        for rx in receivers {
            if let Ok(hits) = rx.recv().await {
                for m in hits {
                    if seen.insert((m.mailbox.clone(), m.uid)) {
                        results.push(m);
                    }
                }
            }
        }
        // The query changed (or search was left entirely) while these were
        // in flight - the reply is for a search that's no longer current.
        if state.borrow().search_query != query {
            return;
        }
        state.borrow_mut().search_results = results;
        repopulate_search_results(&state, &message_list);
    });
}

/// Leaves full-text search: clears the query entry and restores the
/// pre-search view - the open mailbox (`MailView::Single`, repopulated from
/// its cache with a fresh sync requested) or the unified "All Inboxes" view
/// (`MailView::UnifiedInbox`, repopulated from the per-mailbox snapshots). A
/// no-op when no search is active, aside from clearing an idle (empty) entry.
fn exit_search(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    message_list: &MessageListModel,
    message_list_stack: &gtk::Stack,
    list_header: &ListHeader,
    search_entry: &gtk::SearchEntry,
) {
    if !state.borrow().search_active {
        search_entry.set_text("");
        return;
    }
    let restored_view = {
        let mut st = state.borrow_mut();
        st.search_active = false;
        st.search_query.clear();
        st.search_results.clear();
        st.search_pending.clear();
        if st.current_mailbox.is_some() {
            MailView::Single
        } else {
            MailView::UnifiedInbox
        }
    };
    state.borrow_mut().mail_view = restored_view;
    search_entry.set_text("");
    refresh_list_header(state, list_header);
    if restored_view == MailView::UnifiedInbox {
        for (account_id, inbox_id) in account_inboxes(state) {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
        let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
    } else {
        // Single view: paint from the mailbox's cache now so the list doesn't
        // linger on search results, then let the requested sync refresh it.
        let (account_id, mailbox_id) = {
            let st = state.borrow();
            (st.current_account.clone(), st.current_mailbox.clone())
        };
        if let Some((account_id, mailbox_id)) = account_id.zip(mailbox_id) {
            request_mailbox_sync(state, &account_id, &mailbox_id);
            if let Some(cache) = state.borrow().accounts.get(&account_id).and_then(|h| h.address_cache.clone()) {
                let reply_rx = spawn_cache_read(worker, cache, {
                    let mailbox_id = mailbox_id.clone();
                    move |cache| cache.load_messages(&mailbox_id).ok()
                });
                let state = state.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                glib::spawn_future_local(async move {
                    let Ok(Some(messages)) = reply_rx.recv().await else { return };
                    // Discard a reply for a mailbox the user has since
                    // navigated away from.
                    if state.borrow().current_mailbox.as_ref() != Some(&mailbox_id) {
                        return;
                    }
                    let (key, descending) = current_sort(&state);
                    message_list.repopulate(messages, key, descending);
                    // See `select_mailbox`'s identical call: without this the
                    // spinner set below (before this reply lands) only clears
                    // once the live IMAP sync's `MessagesUpdated` arrives,
                    // even though the cache already painted the real rows.
                    refresh_message_loading_state(&state, &message_list, &message_list_stack);
                });
            }
        }
    }
    refresh_message_loading_state(state, message_list, message_list_stack);
}

/// Enters the "All Inboxes" view: asks every connected account that has an
/// Inbox to sync it, and immediately repopulates the list from whatever the
/// per-mailbox snapshots already hold.
fn enter_unified_inbox(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, message_list_stack: &gtk::Stack) {
    let inboxes = account_inboxes(state);
    {
        let mut st = state.borrow_mut();
        st.mail_view = MailView::UnifiedInbox;
        // No single mailbox owns the view; leave these unset so a stray
        // single-mailbox `MessagesUpdated` can't match, and the startup
        // adopt-first logic can't override the unified view.
        st.current_account = None;
        st.current_mailbox = None;
        st.restore_pending = false;
        last_view::save(&st.settings, &LastSelection { unified: true, mailbox: None });
    }
    for (account_id, inbox_id) in inboxes {
        request_mailbox_sync(state, &account_id, &inbox_id);
    }
    let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
    refresh_message_loading_state(state, message_list, message_list_stack);
}

/// Merges the unified view's per-mailbox snapshots into a single newest-first
/// list, deduplicating by `(mailbox, uid)` - a message lives in exactly one
/// mailbox, but a mailbox can be re-synced more than once, so the same
/// `(mailbox, uid)` must never appear twice.
fn merge_unified_snapshots(snapshots: &HashMap<MailboxId, Vec<EmailSummary>>) -> Vec<EmailSummary> {
    let mut seen: HashSet<(MailboxId, Uid)> = HashSet::new();
    let mut merged: Vec<EmailSummary> = Vec::new();
    for messages in snapshots.values() {
        for m in messages {
            if seen.insert((m.mailbox.clone(), m.uid)) {
                merged.push(m.clone());
            }
        }
    }
    merged.sort_by(unified_merge_order);
    merged
}

/// The active sort, snapshotted so callers don't hold a `state` borrow across
/// a `repopulate_message_list` (whose splice re-enters the selection handler).
fn current_sort(state: &Rc<RefCell<UiState>>) -> (SortKey, bool) {
    let st = state.borrow();
    (st.sort_key, st.sort_descending)
}

/// Re-orders the visible list under the current sort. The rebuild goes through
/// `MessageListModel::repopulate`, so the selected message stays selected and
/// the reading pane doesn't re-render (and re-crossfade) the email it's
/// already showing.
fn resort_message_list(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel) {
    let messages = message_list.all_messages();
    if messages.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    message_list.repopulate(messages, key, descending);
}

/// Removes `uids` from `mailbox` in the message list immediately, ahead of
/// the server-side move just sent for them actually completing - that move
/// takes several sequential live IMAP round trips (`move_uids_to_path` in
/// `lookout-mail`'s session actor), and waiting for it to finish before
/// hiding the row is the ~2s lag a delete/archive/report used to have. The
/// removed rows are stashed in `state.pending_optimistic_removals` so a
/// matching `AccountEvent::MoveFailed` can restore them; a later
/// `MessagesUpdated` for this mailbox (which the success path always
/// eventually produces) clears the stash instead, since the authoritative
/// sync already reflects reality.
fn optimistic_remove_messages(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let (kept, removed): (Vec<EmailSummary>, Vec<EmailSummary>) = message_list.all_messages().into_iter().partition(|m| !(m.mailbox == *mailbox && uids.contains(&m.uid)));
    if removed.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    message_list.repopulate(kept, key, descending);
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        snapshot.retain(|m| !uids.contains(&m.uid));
    }
    st.pending_optimistic_removals.entry(mailbox.clone()).or_default().extend(removed);
}

/// Undoes `optimistic_remove_messages` for `mailbox`/`uids` after an
/// `AccountEvent::MoveFailed` - puts the stashed rows back into both the
/// visible list and the unified-view snapshot they were pulled from.
fn restore_optimistic_removals(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let restored: Vec<EmailSummary> = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_optimistic_removals.get_mut(mailbox) else { return };
        let mut restored = Vec::new();
        pending.retain(|m| {
            if uids.contains(&m.uid) {
                restored.push(m.clone());
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            st.pending_optimistic_removals.remove(mailbox);
        }
        restored
    };
    if restored.is_empty() {
        return;
    }
    if let Some(snapshot) = state.borrow_mut().unified_snapshots.get_mut(mailbox) {
        snapshot.extend(restored.iter().cloned());
    }
    let mut all = message_list.all_messages();
    all.extend(restored);
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
}

/// Optimistically flips `SystemFlagBit::Seen` for `uids` in `mailbox` -
/// added when marking read, removed when marking unread - and repaints
/// immediately. The pre-toggle summaries are stashed in
/// `pending_optimistic_flag_changes` so a matching `StoreFlagsFailed` can
/// restore exactly them; a later `MessagesUpdated` for this mailbox clears
/// the stash on its own, since the success path already patched the cache
/// to match before emitting it.
fn optimistic_toggle_read(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid], mark_read: bool) {
    let mut before = Vec::new();
    let all: Vec<EmailSummary> = message_list
        .all_messages()
        .into_iter()
        .map(|mut m| {
            if m.mailbox == *mailbox && uids.contains(&m.uid) {
                before.push(m.clone());
                if mark_read {
                    m.flags.insert(SystemFlagBit::Seen);
                } else {
                    m.flags.remove(&SystemFlagBit::Seen);
                }
            }
            m
        })
        .collect();
    if before.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut().filter(|m| uids.contains(&m.uid)) {
            if mark_read {
                m.flags.insert(SystemFlagBit::Seen);
            } else {
                m.flags.remove(&SystemFlagBit::Seen);
            }
        }
    }
    st.pending_optimistic_flag_changes.entry(mailbox.clone()).or_default().extend(before);
}

/// Undoes `optimistic_toggle_read` for `mailbox`/`uids` after a
/// `StoreFlagsFailed` - puts the stashed pre-toggle summaries back in place
/// (by uid) in both the visible list and the unified-view snapshot.
fn restore_optimistic_flag_changes(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, mailbox: &MailboxId, uids: &[Uid]) {
    let restored: HashMap<Uid, EmailSummary> = {
        let mut st = state.borrow_mut();
        let Some(pending) = st.pending_optimistic_flag_changes.get_mut(mailbox) else { return };
        let mut restored = HashMap::new();
        pending.retain(|m| {
            if uids.contains(&m.uid) {
                restored.insert(m.uid, m.clone());
                false
            } else {
                true
            }
        });
        if pending.is_empty() {
            st.pending_optimistic_flag_changes.remove(mailbox);
        }
        restored
    };
    if restored.is_empty() {
        return;
    }
    let mut st = state.borrow_mut();
    if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
        for m in snapshot.iter_mut() {
            if let Some(orig) = restored.get(&m.uid) {
                m.flags = orig.flags.clone();
            }
        }
    }
    drop(st);
    let all: Vec<EmailSummary> = message_list.all_messages().into_iter().map(|m| restored.get(&m.uid).cloned().unwrap_or(m)).collect();
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
}

/// Groups every currently-selected message in `message_list` by the
/// `(account, mailbox)` it lives in, for the batch Delete/Archive/Report/
/// Snooze/Flag/Mark-read button handlers - mirrors the lookup already done
/// inline by the `FetchBody`-on-selection handler above, generalized from
/// one message to a set of them. The account is derived from each message's
/// own `MailboxId` rather than the view's `current_account`, so the unified
/// "All Inboxes" list routes each message to the right account regardless of
/// which mailboxes are mixed into the selection.
///
/// Returns one entry per distinct mailbox touched - so a batch action sends
/// exactly one plural `AccountCommand` per mailbox, not one per message -
/// with each group's uids in selection order. A message whose account has
/// since disconnected is silently dropped from its group rather than failing
/// the whole batch, same convention as this function's single-message
/// predecessor. Empty (nothing selected, or every account disconnected)
/// yields an empty `Vec`, which every caller below treats as a no-op loop.
///
/// For exactly one selected message this returns exactly one entry with
/// `uids == vec![uid]` - a strict, behavior-preserving generalization of the
/// function it replaces.
fn selected_message_command_targets(message_list: &MessageListModel, state: &Rc<RefCell<UiState>>) -> Vec<(async_channel::Sender<AccountCommand>, MailboxId, Vec<Uid>)> {
    let summaries = message_list.selected_summaries();
    let st = state.borrow();
    let mut groups: HashMap<(AccountId, MailboxId), (async_channel::Sender<AccountCommand>, Vec<Uid>)> = HashMap::new();
    for summary in summaries {
        let Some(account_id) = mailbox_account_id(&summary.mailbox) else { continue };
        let Some(handle) = st.accounts.get(&account_id) else { continue };
        groups
            .entry((account_id, summary.mailbox.clone()))
            .or_insert_with(|| (handle.cmd_tx.clone(), Vec::new()))
            .1
            .push(summary.uid);
    }
    groups.into_iter().map(|((_, mailbox), (cmd_tx, uids))| (cmd_tx, mailbox, uids)).collect()
}

/// Resolves the currently-selected message plus its already-fetched body,
/// for the Reply/Reply-All/Forward button handlers. Returns `None` if
/// nothing is selected, a section header is selected, its account has
/// disconnected, or the selected message's body isn't in the in-memory cache
/// (its body hasn't arrived yet, or it's outside the cache's bounded recency
/// window) - the calling handler is then a silent no-op, same convention as
/// `selected_message_command_target`. The
/// "From" identity is the message's owning account, so replies composed from
/// the unified view go out from the right address.
fn selected_message_reply_context(
    message_list: &MessageListModel,
    state: &Rc<RefCell<UiState>>,
) -> Option<(EmailSummary, EmailBody, String, async_channel::Sender<AccountCommand>)> {
    let summary = message_list.selected_summary()?;

    let account_id = mailbox_account_id(&summary.mailbox)?;
    let mut st = state.borrow_mut();
    let body = st.body_cache.get(&summary.mailbox, &summary.uid)?;
    let handle = st.accounts.get(&account_id)?;
    Some((summary, body, handle.email.clone(), handle.cmd_tx.clone()))
}

fn account_label(state: &Rc<RefCell<UiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

/// The snapshot `rebuild_folder_tree` takes of `UiState` before touching the
/// selection: whether to auto-select the first Inbox, then each account's
/// (id, label, folders), then the resolved starred mailboxes.
type FolderTreeSnapshot = (bool, Vec<(AccountId, String, Vec<Mailbox>)>, Vec<Mailbox>);

/// Exactly the data the sidebar draws - per account its id and label, and per
/// folder the id, display name, icon-selecting role, delimiter (the tree's
/// parent/child structure is derived from it) and unread count - plus the
/// favorites section's membership. Compared against the previous rebuild's
/// value to decide whether a `FoldersUpdated` needs a rebuild at all.
///
/// Deliberately *not* the whole `Mailbox`: a STATUS refresh writes `uidnext`
/// and `uidvalidity` on every pass, and including fields the tree never
/// renders would make the guard fire on changes that can't be seen.
type FolderTreeSignature = Vec<(AccountId, String, Vec<(MailboxId, String, MailboxRole, char, u32)>)>;

fn folder_tree_signature(accounts: &[(AccountId, String, Vec<Mailbox>)], favorites: &[Mailbox]) -> FolderTreeSignature {
    let row = |m: &Mailbox| (m.id.clone(), m.name.clone(), m.role, m.delimiter, m.unread);
    let mut signature: FolderTreeSignature = accounts
        .iter()
        .map(|(id, label, folders)| (id.clone(), label.clone(), folders.iter().map(row).collect()))
        .collect();
    // The favorites section is a real part of the rendered tree (it appears
    // and disappears with its membership), so it belongs in the signature.
    // Carried under a reserved pseudo-account id rather than a separate field
    // so the comparison stays a single `==`.
    signature.push((AccountId(FAVORITES_GROUP_KEY.into()), String::new(), favorites.iter().map(row).collect()));
    signature
}

/// Rebuilds the folder sidebar's `Gtk.TreeListModel` from every connected
/// account's latest folder snapshot. Accounts are sorted by email for a
/// stable order across rebuilds (`HashMap` iteration order isn't stable,
/// and accounts visibly reshuffling on every resync would be jarring).
/// On startup - before any folder has been selected or message adopted -
/// the pane opens on the user's remembered view (see `last_view`), or the
/// "All Inboxes" unified row by default (see
/// `restore_or_default_initial_view`).
fn rebuild_folder_tree(state: &Rc<RefCell<UiState>>, folder_selection: &gtk::SingleSelection, folder_scroller: &gtk::ScrolledWindow) {
    // Borrow only long enough to snapshot the account data. `set_selected`
    // inside `select_first_inbox` synchronously fires the `selected-item`
    // handler, which itself borrows `state` mutably - so no borrow may be
    // live across that call.
    let (auto_select_inbox, mut accounts, mut favorites): FolderTreeSnapshot = {
        let st = state.borrow();
        let accounts = st
            .accounts
            .iter()
            .map(|(id, handle)| {
                let label = if handle.display_name.is_empty() {
                    handle.email.clone()
                } else {
                    handle.display_name.clone()
                };
                (id.clone(), label, handle.folders.clone())
            })
            .collect();
        // Starred mailboxes are held as bare ids; resolve each against its
        // account's current folder list, dropping any whose folder has since
        // disappeared (or whose account hasn't connected yet).
        let favorites = st
            .favorites
            .iter()
            .filter_map(|mailbox_id| {
                let account_id = mailbox_account_id(mailbox_id)?;
                let handle = st.accounts.get(&account_id)?;
                handle.folders.iter().find(|m| &m.id == mailbox_id).cloned()
            })
            .collect();
        // Don't yank a user already in the unified view back to the first
        // account's Inbox just because some account resynced its folders.
        let auto_select_inbox = st.current_mailbox.is_none() && !matches!(st.mail_view, MailView::UnifiedInbox);
        (auto_select_inbox, accounts, favorites)
    };
    accounts.sort_by_key(|a| a.1.to_lowercase());
    favorites.sort_by_key(|m| m.name.to_lowercase());

    // While the unified view is active, any account whose Inbox just appeared
    // (or reconnected) gets asked to sync so it populates the merged list.
    // Mailboxes with a sync already outstanding are skipped (`syncing`).
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        let missing: Vec<(AccountId, MailboxId)> = {
            let st = state.borrow();
            st.accounts
                .iter()
                .filter_map(|(id, handle)| {
                    handle
                        .folders
                        .iter()
                        .find(|m| matches!(m.role, MailboxRole::Inbox))
                        .filter(|m| !st.unified_snapshots.contains_key(&m.id) && !st.syncing.contains(&m.id))
                        .map(|m| (id.clone(), m.id.clone()))
                })
                .collect()
        };
        for (account_id, inbox_id) in missing {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
    }

    // Nothing the sidebar draws has changed - most `FoldersUpdated` events
    // are now folder-count refreshes for folders whose counts didn't move, so
    // this is the common case. Swapping in an identical model is not free:
    // it rebuilds every row, collapses whatever subfolders the user had
    // expanded, and drops the selection (see the restore below), so skipping
    // it outright is what keeps the pane still while counts fill in.
    let signature = folder_tree_signature(&accounts, &favorites);
    {
        let st = state.borrow();
        if st.folder_tree.as_ref() == Some(&signature) && !auto_select_inbox && !st.restore_pending {
            return;
        }
    }

    // The selection, the expanded subfolders, the account groups' collapse
    // state, and the scroll position don't survive `set_model`, so note them
    // all first and put them back afterwards. The scroll value matters too:
    // `GtkSingleSelection` autoselects row 0 ("All Inboxes") on `set_model`,
    // and without a restore the pane can end up jumped to the top, leaving a
    // scrolled-down account's subtree looking collapsed.
    let expanded = expanded_mailboxes(folder_selection);
    let collapsed_groups = collapsed_account_groups(folder_selection);
    let scroll_value = folder_scroller.vadjustment().value();
    let restore_to = {
        let st = state.borrow();
        match st.mail_view {
            // Row 0 is always the "All Inboxes" row.
            MailView::UnifiedInbox => Some(SelectionTarget::Unified),
            // During a search the folder pane still highlights the mailbox the
            // search started from (or nothing, if it started from the unified
            // view).
            MailView::Single | MailView::Search => st.current_mailbox.clone().map(SelectionTarget::Mailbox),
        }
    };

    let model = build_multi_account_tree_model(accounts, favorites);
    // Everything from here to the flag being cleared is the rebuild putting
    // the pane back the way the user left it, not the user navigating - so
    // the `selected-item` handler has to stay out of it. `set_model` alone
    // fires it, autoselecting row 0 and thereby entering the unified view.
    state.borrow_mut().suppress_folder_selection = true;
    folder_selection.set_model(Some(&model));
    expand_mailboxes(&model, &expanded);
    apply_account_group_expansion(&model, &collapsed_groups);
    folder_scroller.vadjustment().set_value(scroll_value);
    match restore_to {
        Some(SelectionTarget::Unified) => folder_selection.set_selected(0),
        Some(SelectionTarget::Mailbox(mailbox)) => {
            if let Some(index) = find_mailbox_index(&model, &mailbox) {
                folder_selection.set_selected(index);
            }
        }
        None => {}
    }
    state.borrow_mut().suppress_folder_selection = false;

    state.borrow_mut().folder_tree = Some(signature);
    if auto_select_inbox {
        restore_or_default_initial_view(state, &model, folder_selection);
    }
}

/// Where `rebuild_folder_tree` puts the sidebar highlight back after swapping
/// the model: the pinned "All Inboxes" row, or the open mailbox's own row.
enum SelectionTarget {
    Unified,
    Mailbox(MailboxId),
}

/// The mailboxes whose rows are currently expanded, so a rebuild can restore
/// them. Only `TreeItem::Folder` rows are collected - account groups and the
/// Favorites section carry their own collapse state (see
/// `collapsed_account_groups`), and `Favorite` rows are leaves.
fn expanded_mailboxes(folder_selection: &gtk::SingleSelection) -> HashSet<MailboxId> {
    let mut expanded = HashSet::new();
    let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() else {
        return expanded;
    };
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
        if !row.is_expanded() {
            continue;
        }
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
        let tree_item = boxed.borrow::<TreeItem>();
        if let TreeItem::Folder(node) = &*tree_item {
            expanded.insert(node.mailbox.id.clone());
        }
    }
    expanded
}

/// Re-expands the rows named by `expanded` in a freshly built model. Walks by
/// index rather than over a snapshot because expanding a row inserts its
/// children into the model, and those children may themselves need expanding.
fn expand_mailboxes(model: &gtk::TreeListModel, expanded: &HashSet<MailboxId>) {
    if expanded.is_empty() {
        return;
    }
    let mut i = 0;
    while i < model.n_items() {
        if let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() {
            if let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() {
                let is_wanted = matches!(&*boxed.borrow::<TreeItem>(), TreeItem::Folder(node) if expanded.contains(&node.mailbox.id));
                if is_wanted {
                    row.set_expanded(true);
                }
            }
        }
        i += 1;
    }
}

/// The reserved pseudo-account key under which the Favorites section's group
/// state is carried by `collapsed_account_groups` - the same sentinel
/// `folder_tree_signature` uses to fold the favorites section into its
/// comparison.
const FAVORITES_GROUP_KEY: &str = "\u{0}favorites";

/// The account groups (and the Favorites section) the user has collapsed in
/// the current tree, so a rebuild can keep them collapsed - the account-level
/// counterpart of `expanded_mailboxes`, which covers subfolders only.
///
/// Only rows that are genuinely collapsible are recorded: an account whose
/// folder list hasn't arrived yet renders as a row that can't have been
/// collapsed by the user, so it must never be captured - otherwise a slow
/// account's pre-connect state would be remembered as "the user collapsed
/// it", and its subtree would reopen collapsed once its folders landed. The
/// Favorites section is keyed under `FAVORITES_GROUP_KEY`, mirroring
/// `folder_tree_signature`.
fn collapsed_account_groups(folder_selection: &gtk::SingleSelection) -> HashSet<AccountId> {
    let mut collapsed = HashSet::new();
    let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() else {
        return collapsed;
    };
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
        if row.depth() != 0 || row.is_expanded() || !row.is_expandable() {
            continue;
        }
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
        let tree_item = boxed.borrow::<TreeItem>();
        match &*tree_item {
            TreeItem::Account(acc) => {
                collapsed.insert(acc.account_id.clone());
            }
            TreeItem::Favorites => {
                collapsed.insert(AccountId(FAVORITES_GROUP_KEY.into()));
            }
            _ => {}
        }
    }
    collapsed
}

/// Applies the account groups' (and the Favorites section's) collapse state
/// to a freshly built model, replacing `build_multi_account_tree_model`'s old
/// unconditional expand-all. Rows the user collapsed stay collapsed;
/// everything else - including accounts that just connected - defaults to
/// expanded, the long-standing look of the pane. Only depth-0 rows are
/// touched: subfolders are the caller's `expand_mailboxes` restore's job.
/// Walks by index with `n_items` re-evaluated (like `expand_mailboxes`)
/// because expanding a row inserts its children into the flat model, shifting
/// every later row's position; a fixed range would silently skip them.
fn apply_account_group_expansion(model: &gtk::TreeListModel, collapsed: &HashSet<AccountId>) {
    let mut i = 0;
    while i < model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else {
            i += 1;
            continue;
        };
        if row.depth() != 0 {
            i += 1;
            continue;
        }
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
            i += 1;
            continue;
        };
        let tree_item = boxed.borrow::<TreeItem>();
        let key = match &*tree_item {
            TreeItem::Account(acc) => Some(acc.account_id.clone()),
            TreeItem::Favorites => Some(AccountId(FAVORITES_GROUP_KEY.into())),
            _ => None,
        };
        if let Some(key) = key {
            if !collapsed.contains(&key) {
                row.set_expanded(true);
            }
        }
        i += 1;
    }
}

/// Chooses the folder pane's starting selection on first load. A remembered
/// view (see `last_view`) is restored when its mailbox is present in the
/// rebuilt tree; a remembered "All Inboxes" restores the unified row. With
/// no memory the pane defaults to "All Inboxes" (row 0). If the remembered
/// mailbox's account hasn't connected yet, the restore is deferred - this
/// does nothing and `rebuild_folder_tree` runs it again on the next
/// `FoldersUpdated`, so a slow account can't lose the user's folder. Only
/// once that account has delivered folders and still doesn't list the
/// mailbox is the restore abandoned for the unified default.
fn restore_or_default_initial_view(state: &Rc<RefCell<UiState>>, model: &gtk::TreeListModel, folder_selection: &gtk::SingleSelection) {
    // Decide without holding a borrow: `set_selected` synchronously fires the
    // `selected-item` handler, which itself borrows `state` mutably.
    let (action, done) = {
        let mut st = state.borrow_mut();
        let action = match st.last_selection.clone() {
            None => Some(0),
            Some(sel) if sel.unified => Some(0),
            Some(sel) => match sel.mailbox {
                Some(mailbox) => {
                    let mailbox_id = MailboxId(mailbox);
                    if let Some(index) = find_mailbox_index(model, &mailbox_id) {
                        Some(index)
                    } else {
                        // Not visible yet. If the mailbox's account has
                        // connected and still doesn't list it, the folder is
                        // gone - fall back to the unified default rather than
                        // waiting forever. Otherwise keep waiting for the
                        // account to connect.
                        let account_gone = mailbox_account_id(&mailbox_id)
                            .and_then(|id| st.accounts.get(&id))
                            .map(|h| !h.folders.is_empty())
                            .unwrap_or(false);
                        account_gone.then_some(0)
                    }
                }
                None => Some(0),
            },
        };
        let done = action.is_some();
        if done {
            st.restore_pending = false;
        }
        (action, done)
    };
    if done {
        if let Some(index) = action {
            folder_selection.set_selected(index);
        }
    }
}

/// Finds the flattened index of a specific mailbox row in the folder tree's
/// `TreeListModel`, by exact `MailboxId`. On the flat `TreeListModel`,
/// iterating top-level indices reaches folder rows inside expanded account
/// groups. Used to restore the remembered view on startup and to put the
/// highlight back after a favorite toggle rebuilds the tree.
///
/// Deliberately matches only `TreeItem::Folder`: a starred mailbox also has a
/// `TreeItem::Favorite` row in the pinned Favorites section, which sorts
/// *above* its real row, so matching both would resolve every favorite to its
/// duplicate.
fn find_mailbox_index(model: &gtk::TreeListModel, mailbox_id: &MailboxId) -> Option<u32> {
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else {
            continue;
        };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
            continue;
        };
        let tree_item = boxed.borrow::<TreeItem>();
        if let TreeItem::Folder(node) = &*tree_item {
            if &node.mailbox.id == mailbox_id {
                return Some(i);
            }
        }
    }
    None
}

/// Wraps `content` directly in a `.card`-styled box - libadwaita's real,
/// built-in style class for a rounded-corner, softly-shaded panel. No
/// per-section header bar: `Gtk.Paned` (see `build_window`) provides the
/// resize handle and visual gap between cards, and the small margin here is
/// what makes each card's rounded corners actually visible against the
/// window background instead of touching the divider/edges.
fn card_section(content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"])
        .overflow(gtk::Overflow::Hidden)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    card.append(content);
    card
}

/// Config → Google Tasks → "OAuth client id": a small modal dialog with an
/// entry prefilled from the current configuration. Saving writes the config
/// file (an environment variable still takes precedence at runtime, but the
/// file is what a GUI app can actually manage); clearing removes the file.
fn show_google_tasks_client_id_dialog(window: &adw::ApplicationWindow, toast_overlay: &adw::ToastOverlay, row: &adw::ActionRow) {
    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Google Tasks OAuth client id")
        .default_width(600)
        .build();

    let help = gtk::Label::builder()
        .label(
            "Register a Desktop OAuth client in Google Cloud (console.cloud.google.com → APIs & Services → Credentials) \
             and paste its client id here. The sign-in step needs the client's redirect to http://localhost.",
        )
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .build();
    let entry = gtk::Entry::builder().placeholder_text("e.g. 1234567890-abc.apps.googleusercontent.com").build();
    entry.set_text(&google_tasks::configured_client_id());

    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let top_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    top_bar.append(&cancel_button);
    top_bar.append(&save_button);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&help);
    content.append(&entry);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&top_bar);
    toolbar_view.set_content(Some(&content));
    dialog.set_content(Some(&toolbar_view));

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let row = row.clone();
        let toast_overlay = toast_overlay.clone();
        save_button.connect_clicked(move |_| {
            let id = entry.text().trim().to_string();
            if id.is_empty() {
                let _ = std::fs::remove_file(google_tasks::client_id_file_path());
                toast_overlay.add_toast(adw::Toast::new("Google Tasks client id cleared"));
            } else if let Err(e) = google_tasks::set_client_id(&id) {
                let title = glib::markup_escape_text(&format!("Couldn't save client id: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
                return;
            } else {
                toast_overlay.add_toast(adw::Toast::new("Google Tasks client id saved"));
            }
            crate::config_view::refresh_google_tasks_client_row(&row);
            dialog.close();
        });
    }

    dialog.present();
}

/// Maps a mailbox's special-use role to a folder-row icon name, mirroring the
/// role->icon mapping in the reference webmail app's `getIconForMailbox()`
/// (webmail/components/layout/sidebar.tsx). These specific names come from
/// this machine's Yaru icon theme, not the base Adwaita theme - fine for now
/// since there's no working Flatpak build yet, just a manifest spike.
fn folder_icon_name(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "mail-inbox-symbolic",
        MailboxRole::Sent => "mail-sent-symbolic",
        MailboxRole::Drafts => "mail-drafts-symbolic",
        MailboxRole::Trash => "user-trash-symbolic",
        MailboxRole::Junk => "mail-spam-symbolic",
        MailboxRole::Archive => "mail-archive-symbolic",
        MailboxRole::Custom => "folder-symbolic",
    }
}

fn body_request_matches(mailbox: &MailboxId, uid: &Uid, pending_request: Option<&(MailboxId, Uid)>) -> bool {
    pending_request.is_some_and(|(pending_mailbox, pending_uid)| pending_mailbox == mailbox && pending_uid == uid)
}

/// Finishes a WebKit URI-scheme request with an error, so the browser renders
/// a broken image instead of hanging the page's load on the subresource.
fn finish_cid_request_error(request: &webkit::URISchemeRequest, message: &str) {
    request.finish_error(&mut glib::Error::new(gio::IOErrorEnum::Failed, message));
}

/// Finishes every in-flight inline `cid:` request with an error - called
/// whenever the reading pane moves off the message those requests belong to
/// (new selection, new render, account teardown), so WebKit never waits on
/// subresources of a message the user has left.
fn drop_pending_cid(state: &Rc<RefCell<UiState>>) {
    let pending = std::mem::take(&mut state.borrow_mut().pending_cid);
    for (part_number, pending) in pending {
        tracing::debug!(part = %part_number, "cid: dropping pending inline-image request");
        finish_cid_request_error(&pending.request, "the message changed before the inline image arrived");
    }
}

/// Resolves a `cid:` reference requested by the reading pane's WebKit view
/// to the matching inline part of the currently-rendered message, and asks
/// that account's session for the part's bytes (`FetchAttachment`). Runs on
/// the main loop (the scheme handler itself only forwards the request here,
/// because it fires on a WebKit worker thread). A reference that matches no
/// part of the current message is answered with an error immediately, so an
/// unknown id can't wedge the pane; so is a reference arriving while no
/// message is on screen. Each dispatched request is tracked in
/// `UiState::pending_cid` until `PartFetched`/`PartFetchFailed` (or the
/// timeout) lands.
fn dispatch_cid_request(state: &Rc<RefCell<UiState>>, cid: &str, request: webkit::URISchemeRequest) {
    let target = (|| {
        let st = state.borrow();
        let (mailbox, uid) = st.rendered_message.clone()?;
        let part = st
            .rendered_inline_parts
            .iter()
            .find(|p| p.cid.as_deref().is_some_and(|c| lookout_core::cid_matches(cid, c)))?
            .clone();
        let cmd_tx = mailbox_account_id(&mailbox).and_then(|id| st.accounts.get(&id)).map(|h| h.cmd_tx.clone())?;
        Some((mailbox, uid, part, cmd_tx))
    })();
    let Some((mailbox, uid, part, cmd_tx)) = target else {
        finish_cid_request_error(&request, "this message has no matching inline image");
        return;
    };
    let part_number = part.part_number.clone();
    state.borrow_mut().pending_cid.insert(
        part_number.clone(),
        PendingCid {
            mailbox: mailbox.clone(),
            uid,
            request,
        },
    );
    tracing::debug!(?mailbox, uid = uid.0, cid, part = %part_number, "cid: inline image request dispatched to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchAttachment { mailbox, uid, part });
    arm_cid_timeout(state, part_number);
}

/// Backstop for a `cid:` image fetch whose answer is lost (e.g. the session
/// dies mid-fetch and the command disappears into the reconnect): finish the
/// WebKit request with an error after a generous grace period instead of
/// letting the page hang on it. Only fires if this exact request is still
/// the outstanding one.
fn arm_cid_timeout(state: &Rc<RefCell<UiState>>, part_number: String) {
    let state_for_timeout = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let request = {
            let mut st = state_for_timeout.borrow_mut();
            match st.pending_cid.get(&part_number) {
                Some(_) => st.pending_cid.remove(&part_number).expect("present").request,
                None => return glib::ControlFlow::Break,
            }
        };
        tracing::warn!(part = %part_number, "cid: inline image fetch timed out");
        finish_cid_request_error(&request, "the inline image could not be fetched");
        glib::ControlFlow::Break
    });
}

/// Reveals the reading pane's "message" page (switching `content_stack` to
/// `content_page` and the reading stack to "message"). If the stack is
/// mid-transition - e.g. a message → "empty" fade-out still settling after a
/// fast body load - the reveal is deferred until that transition completes,
/// so the next message never appears before the previous one has fully faded
/// away.
fn reveal_message_page(reading_stack: &gtk::Stack, content_stack: &gtk::Stack, content_page: &'static str) {
    if reading_stack.is_transition_running() {
        let reading_stack = reading_stack.clone();
        let content_stack = content_stack.clone();
        let settled = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
        let settled_for_notify = settled.clone();
        let handler_id = reading_stack.connect_notify_local(Some("transition-running"), move |stack, _| {
            if !stack.is_transition_running() {
                if let Some(handler_id) = settled_for_notify.take() {
                    stack.disconnect(handler_id);
                }
                content_stack.set_visible_child_name(content_page);
                stack.set_visible_child_name("message");
            }
        });
        settled.set(Some(handler_id));
    } else {
        content_stack.set_visible_child_name(content_page);
        reading_stack.set_visible_child_name("message");
    }
}

/// Locates a named child of the reading pane's `"message"` page by walking
/// its children - used to find widgets by `widget_name()` rather than relying
/// on their position among siblings, since the page mixes fixed structural
/// children (attachment strip, body stack) with ones the header/action-bar
/// widgets add around them.
fn find_named_child(reading_stack: &gtk::Stack, name: &str) -> Option<gtk::Widget> {
    let page = reading_stack.child_by_name("message").and_downcast::<gtk::Box>()?;
    let mut child = page.first_child();
    while let Some(c) = child {
        if c.widget_name() == name {
            return Some(c);
        }
        child = c.next_sibling();
    }
    None
}

/// Locates the reading pane's attachment strip - the named `gtk::Box`
/// (`"attachments"`) that `build_window` inserted between the message header
/// and the body `content_stack`.
fn find_attachment_strip(reading_stack: &gtk::Stack) -> Option<gtk::Box> {
    find_named_child(reading_stack, "attachments")?.downcast::<gtk::Box>().ok()
}

/// Returns the first direct child of `root` whose `widget_name()` is `name`.
/// The invite card's fixed row structure is looked up this way (rather than
/// holding widget references across `render_body` calls) so `render_invite_card`
/// can repopulate the card the same way `rebuild_attachment_strip` finds the
/// strip.
fn named_child_of(root: &gtk::Box, name: &str) -> Option<gtk::Widget> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if c.widget_name() == name {
            return Some(c);
        }
        child = c.next_sibling();
    }
    None
}

/// One row of the reading pane's invite-details card: a dim caption above a
/// wrapping value label. Both the row and its value label get `widget_name`s
/// (`row_name`/`value_name`) so `render_invite_card` can find them and hide a
/// row wholesale when the invitation lacks that property.
fn invite_detail_row(row_name: &str, value_name: &str, caption: &str) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).build();
    row.set_widget_name(row_name);
    let caption_label = gtk::Label::new(Some(caption));
    caption_label.add_css_class("dim-label");
    caption_label.add_css_class("caption");
    caption_label.set_xalign(0.0);
    let value = gtk::Label::new(None);
    value.set_widget_name(value_name);
    value.set_xalign(0.0);
    value.set_wrap(true);
    value.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    row.append(&caption_label);
    row.append(&value);
    row
}

/// Sets one invite-card row's value label, hiding the row when the invitation
/// has nothing for it. `text == None` hides the row; `Some("")` is not
/// expected but would show an empty value (callers only pass `Some` for
/// payloads that actually carry the property).
fn set_invite_row(card: &gtk::Box, row_name: &str, value_name: &str, text: Option<String>) {
    let Some(row) = named_child_of(card, row_name).and_then(|r| r.downcast::<gtk::Box>().ok()) else {
        return;
    };
    row.set_visible(text.is_some());
    if let Some(label) = named_child_of(&row, value_name).and_then(|l| l.downcast::<gtk::Label>().ok()) {
        label.set_label(text.as_deref().unwrap_or(""));
    }
}

/// Renders the invite card's "When" line: the event's start/end in local time
/// (all-day entries as dates, with `DTEND`'s exclusivity undone for the last
/// shown day), plus a recurring hint when the master carries an `RRULE`.
/// Follows the 12-hour style the calendar views and the message header use.
fn format_imip_when(invitation: &lookout_core::ImipInvitation) -> String {
    let recurring = if invitation.rrule.is_some() { " · Recurring" } else { "" };
    let start = invitation.start.with_timezone(&chrono::Local);
    let end = invitation.end.with_timezone(&chrono::Local);
    if invitation.all_day {
        if end.date_naive() == start.date_naive() {
            return format!("All day · {}{}", start.format("%a %d %b %Y"), recurring);
        }
        // DTEND is exclusive: the last day shown is the one before it.
        let last_day = end.date_naive() - chrono::Duration::days(1);
        return format!("All day · {} – {}{}", start.format("%a %d %b %Y"), last_day.format("%a %d %b %Y"), recurring);
    }
    if end.date_naive() == start.date_naive() {
        format!("{} · {} – {}{}", start.format("%a %d %b %Y"), start.format("%I:%M %p"), end.format("%I:%M %p"), recurring)
    } else {
        format!(
            "{} {} – {} {}{}",
            start.format("%a %d %b %Y"),
            start.format("%I:%M %p"),
            end.format("%a %d %b %Y"),
            end.format("%I:%M %p"),
            recurring
        )
    }
}

/// The invite card's "Organizer" line: the display name (when the payload
/// carries one) alongside the address, so the reply's recipient is unambiguous.
fn format_imip_organizer(organizer: &lookout_core::EmailAddress) -> String {
    match &organizer.name {
        Some(name) if !name.trim().is_empty() => format!("{} <{}>", name, organizer.address),
        _ => organizer.address.clone(),
    }
}

/// Repopulates the reading pane's invite-details card for the invitation
/// being rendered, hiding it when the message carries none (or the user
/// dismissed it). Complements the `adw::Banner` at the bottom of the page,
/// which only fits a title and a button.
fn render_invite_card(reading_stack: &gtk::Stack, invitation: Option<&lookout_core::ImipInvitation>) {
    let Some(card) = find_named_child(reading_stack, "imip-invite-card").and_then(|c| c.downcast::<gtk::Box>().ok()) else {
        return;
    };
    card.set_visible(invitation.is_some());
    let Some(invitation) = invitation else { return };
    set_invite_row(&card, "imip-when-row", "imip-when", Some(format_imip_when(invitation)));
    set_invite_row(&card, "imip-where-row", "imip-where", invitation.location.clone());
    set_invite_row(&card, "imip-organizer-row", "imip-organizer", invitation.organizer.as_ref().map(format_imip_organizer));
    set_invite_row(&card, "imip-description-row", "imip-description", invitation.description.clone());
}

/// Renders an attachment's size as a human-readable string (e.g. `"12.3 KB"`),
/// matching the binary-unit convention everyone expects for file sizes.
pub(crate) fn human_size(bytes: u32) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// A safe save-suggested name for an attachment without one: falls back to a
/// part-number-based placeholder (the raw wire could be anything, and a
/// filename containing `/` from a hostile header must not be fed to the save
/// dialog as a path). `filename` is trusted when present - it's just the
/// initial name in the dialog, which the user can change.
fn attachment_display_name(part: &BodyPart) -> String {
    part.filename
        .as_deref()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty() && !f.contains('/') && !f.contains('\\'))
        .map(str::to_string)
        .unwrap_or_else(|| format!("attachment-{}", part.part_number))
}

/// Rebuilds the reading pane's attachment strip for the message about to be
/// rendered: clears whatever's there, then one row per attachment `BodyPart`
/// (`is_attachment` only - inline `cid:` images stay in the HTML body). Each
/// row shows the attachment's name and size plus a menu button whose popover
/// offers **Open** (the MIME type's default handler, via a temporary file
/// deleted when Lookout exits), **Open With…** (a chooser dialog for the
/// handler), and **Save…** (a save-location dialog) - all three ask the
/// account session for that part's bytes on demand (`FetchAttachment`). The
/// strip is hidden when the message has no attachments. One action is in
/// flight at a time, tracked as the single `UiState::pending_attachment`.
fn rebuild_attachment_strip(state: &Rc<RefCell<UiState>>, reading_stack: &gtk::Stack, mailbox: &MailboxId, uid: Uid, parts: &[BodyPart]) {
    let Some(strip) = find_attachment_strip(reading_stack) else { return };
    // A previous render's in-flight action belongs to the message being
    // replaced; any late `PartFetched` for it is discarded as stale.
    state.borrow_mut().pending_attachment = None;
    while let Some(child) = strip.first_child() {
        strip.remove(&child);
    }

    let attachments: Vec<&BodyPart> = parts.iter().filter(|p| p.is_attachment).collect();
    strip.set_visible(!attachments.is_empty());
    for part in attachments {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("attachment-row");
        let icon = gtk::Image::from_icon_name("mail-attachment-symbolic");
        icon.add_css_class("dim-label");
        let name = attachment_display_name(part);
        let label = gtk::Label::new(Some(&format!("{name}  ·  {}", human_size(part.size))));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        let menu_button = gtk::MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.add_css_class("flat");
        menu_button.set_tooltip_text(Some("Save or open attachment"));
        let popover = gtk::Popover::new();
        let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
        items.set_margin_top(6);
        items.set_margin_bottom(6);
        items.set_margin_start(6);
        items.set_margin_end(6);
        let open_item = gtk::Button::with_label("Open");
        open_item.add_css_class("flat");
        open_item.set_halign(gtk::Align::Start);
        let open_with_item = gtk::Button::with_label("Open With…");
        open_with_item.add_css_class("flat");
        open_with_item.set_halign(gtk::Align::Start);
        let save_item = gtk::Button::with_label("Save…");
        save_item.add_css_class("flat");
        save_item.set_halign(gtk::Align::Start);
        items.append(&open_item);
        items.append(&open_with_item);
        items.append(&save_item);
        popover.set_child(Some(&items));
        menu_button.set_popover(Some(&popover));
        row.append(&icon);
        row.append(&label);
        row.append(&menu_button);

        let button_for_open = menu_button.clone();
        let state_for_open = state.clone();
        let mailbox_for_open = mailbox.clone();
        let part_for_open = part.clone();
        open_item.connect_clicked(move |_| {
            start_attachment_fetch(&state_for_open, &mailbox_for_open, uid, &part_for_open, &button_for_open, PendingAttachmentAction::Open);
        });
        // Double-clicking the row is the same as choosing "Open".
        let gesture_open = gtk::GestureClick::new();
        gesture_open.set_button(gtk::gdk::BUTTON_PRIMARY);
        let button_for_dclick = menu_button.clone();
        let state_for_dclick = state.clone();
        let mailbox_for_dclick = mailbox.clone();
        let part_for_dclick = part.clone();
        gesture_open.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                start_attachment_fetch(
                    &state_for_dclick,
                    &mailbox_for_dclick,
                    uid,
                    &part_for_dclick,
                    &button_for_dclick,
                    PendingAttachmentAction::Open,
                );
            }
        });
        row.add_controller(gesture_open);
        let button_for_open_with = menu_button.clone();
        let state_for_open_with = state.clone();
        let mailbox_for_open_with = mailbox.clone();
        let part_for_open_with = part.clone();
        open_with_item.connect_clicked(move |_| {
            start_attachment_fetch(
                &state_for_open_with,
                &mailbox_for_open_with,
                uid,
                &part_for_open_with,
                &button_for_open_with,
                PendingAttachmentAction::OpenWith,
            );
        });
        let button_for_save = menu_button.clone();
        let state_for_save = state.clone();
        let mailbox_for_save = mailbox.clone();
        let part_for_save = part.clone();
        save_item.connect_clicked(move |_| {
            start_attachment_fetch(&state_for_save, &mailbox_for_save, uid, &part_for_save, &button_for_save, PendingAttachmentAction::Save);
        });

        strip.append(&row);
    }
}

/// Starts an attachment action (Save or Open) for one strip row: records it
/// as the single in-flight `UiState::pending_attachment`, disables the row's
/// menu button, and asks the account session for the part's bytes
/// (`AccountCommand::FetchAttachment`). Also arms the 60-second backstop that
/// restores the button if no answer ever arrives (the session dying mid-fetch
/// loses the command to the reconnect) - a resolved or superseded request is
/// left alone.
fn start_attachment_fetch(state: &Rc<RefCell<UiState>>, mailbox: &MailboxId, uid: Uid, part: &BodyPart, button: &gtk::MenuButton, action: PendingAttachmentAction) {
    // One in-flight fetch at a time; ignore a click while one is outstanding.
    if state.borrow().pending_attachment.is_some() {
        return;
    }
    let cmd_tx = match mailbox_account_id(mailbox) {
        Some(id) => {
            let st = state.borrow();
            st.accounts.get(&id).map(|h| h.cmd_tx.clone())
        }
        None => None,
    };
    let Some(cmd_tx) = cmd_tx else { return };
    state.borrow_mut().pending_attachment = Some(PendingAttachment {
        mailbox: mailbox.clone(),
        uid,
        part_number: part.part_number.clone(),
        action,
        button: button.clone(),
    });
    button.set_sensitive(false);
    tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, action = ?action, "attachment action: dispatching to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchAttachment {
        mailbox: mailbox.clone(),
        uid,
        part: part.clone(),
    });
    // Backstop: restore the button after a generous grace period instead of
    // leaving it stuck. Only fires if this exact request is still the
    // outstanding one.
    let timeout_mailbox = mailbox.clone();
    let timeout_part = part.part_number.clone();
    let timeout_button = button.clone();
    let timeout_state = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let still_pending = match &timeout_state.borrow().pending_attachment {
            Some(p) => p.mailbox == timeout_mailbox && p.uid == uid && p.part_number == timeout_part,
            None => false,
        };
        if still_pending {
            let mut st = timeout_state.borrow_mut();
            st.pending_attachment = None;
            drop(st);
            timeout_button.set_sensitive(true);
            if let Some(toast_overlay) = &timeout_state.borrow().toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Attachment fetch timed out"));
            }
        }
        glib::ControlFlow::Break
    });
}

/// Starts the .eml export for the currently-selected message: records it as
/// the single in-flight `UiState::pending_raw_message`, asks the account
/// session for the whole raw message (`AccountCommand::FetchRawMessage`), and
/// arms the same 60-second backstop as attachment fetches. Silent no-op when
/// nothing is selected or the account has disconnected (same convention as
/// the Reply handlers); the save dialog opens once the bytes arrive
/// (`AccountEvent::RawMessageFetched`).
fn start_raw_message_export(message_list: &MessageListModel, state: &Rc<RefCell<UiState>>) {
    let Some(summary) = message_list.selected_summary() else { return };
    let Some(account_id) = mailbox_account_id(&summary.mailbox) else { return };
    let cmd_tx = {
        let st = state.borrow();
        st.accounts.get(&account_id).map(|h| h.cmd_tx.clone())
    };
    let Some(cmd_tx) = cmd_tx else { return };
    // One export in flight at a time; ignore a click while one is outstanding.
    if state.borrow().pending_raw_message.is_some() {
        return;
    }
    state.borrow_mut().pending_raw_message = Some(PendingRawMessage {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
        initial_name: eml_suggested_name(&summary),
    });
    tracing::debug!(?summary.mailbox, uid = summary.uid.0, "raw message export: dispatching to account actor");
    let _ = cmd_tx.send_blocking(AccountCommand::FetchRawMessage {
        mailbox: summary.mailbox.clone(),
        uid: summary.uid,
    });
    // Backstop: clear the pending state after a generous grace period instead
    // of blocking future exports forever (the session dying mid-fetch loses
    // the command to the reconnect). Only fires if this exact request is
    // still the outstanding one.
    let timeout_mailbox = summary.mailbox.clone();
    let timeout_uid = summary.uid;
    let timeout_state = state.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(ATTACHMENT_FETCH_TIMEOUT_MS), move || {
        let still_pending = match &timeout_state.borrow().pending_raw_message {
            Some(p) => p.mailbox == timeout_mailbox && p.uid == timeout_uid,
            None => false,
        };
        if still_pending {
            timeout_state.borrow_mut().pending_raw_message = None;
            if let Some(toast_overlay) = &timeout_state.borrow().toast_overlay {
                toast_overlay.add_toast(adw::Toast::new("Message export timed out"));
            }
        }
        glib::ControlFlow::Break
    });
}

/// Prompts for a save location (via the platform save dialog - a GTK
/// `FileDialog`, which goes through the XDG portal in sandboxed runs) and
/// writes the fetched attachment bytes there. The dialog is cancellable by
/// design; a write failure surfaces as a toast.
async fn save_attachment_to_disk(window: &adw::ApplicationWindow, toast_overlay: adw::ToastOverlay, part: &BodyPart, bytes: &[u8]) {
    let dialog = gtk::FileDialog::builder().title("Save attachment").initial_name(attachment_display_name(part)).build();
    let Ok(file) = dialog.save_future(Some(window)).await else { return };
    let result = file
        .replace_contents_bytes_future(&glib::Bytes::from(bytes), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
        .await;
    match result {
        Ok(_) => toast_overlay.add_toast(adw::Toast::new("Attachment saved")),
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't save attachment: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
        }
    }
}

/// Prompts for a save location and writes a whole raw message there as a
/// `.eml` file - `bytes` are the server's `BODY.PEEK[]` response verbatim, so
/// the file is a valid RFC 5322 message (and the session's raw-message cache
/// makes the export instant on re-save). Mirrors `save_attachment_to_disk`,
/// including its cancel-on-dialog-close behavior and toast feedback.
async fn save_raw_message_to_disk(window: &adw::ApplicationWindow, toast_overlay: adw::ToastOverlay, initial_name: &str, bytes: &[u8]) {
    let filter = gtk::FileFilter::new();
    filter.add_suffix("eml");
    filter.set_name(Some("Email messages (*.eml)"));
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title("Save message as .eml")
        .initial_name(initial_name)
        .filters(&filters)
        .build();
    let Ok(file) = dialog.save_future(Some(window)).await else { return };
    let result = file
        .replace_contents_bytes_future(&glib::Bytes::from(bytes), None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
        .await;
    match result {
        Ok(_) => toast_overlay.add_toast(adw::Toast::new("Message saved as .eml")),
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't save message: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
        }
    }
}

/// A safe suggested filename for the .eml export dialog: the message's
/// subject plus an `.eml` suffix, or a plain placeholder when the subject is
/// empty or contains path separators - the raw subject is untrusted header
/// data and must not be fed to the dialog as a path (same rationale as
/// `attachment_display_name`). Just the initial name; the user can change it
/// in the dialog.
fn eml_suggested_name(summary: &EmailSummary) -> String {
    let subject = summary.subject.as_deref().unwrap_or("").trim();
    if subject.is_empty() || subject.contains('/') || subject.contains('\\') {
        "message.eml".to_string()
    } else {
        format!("{subject}.eml")
    }
}

/// RFC 8058 one-click unsubscribe: POST `List-Unsubscribe=One-Click` to the
/// list's URL. Success is a 2xx (reqwest follows the redirects some lists
/// answer with); a non-2xx response or a transport failure is an error the
/// caller surfaces as a toast (and may fall back to the mailto action).
async fn post_one_click_unsubscribe(url: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .form(&[("List-Unsubscribe", "One-Click")])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("the list answered {}", response.status()))
    }
}

/// Opens the composer pre-filled to send the list's unsubscribe mail - the
/// RFC 2369 `mailto:` fallback for lists without one-click POST (and the
/// fallback when a one-click POST fails). Pre-fills only the recipient and
/// subject, so the user reviews what goes out before anything is sent.
fn open_mailto_unsubscribe(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    address: String,
    cmd_tx: async_channel::Sender<AccountCommand>,
    account_id: Option<AccountId>,
) {
    let prefill = crate::compose::ComposePrefill {
        to: Some(address),
        subject: Some("unsubscribe".to_string()),
        ..Default::default()
    };
    let rich_text_default = state.borrow().rich_text_default;
    show_composer_in_reading_pane(state, worker, reading_stack, "Unsubscribe", cmd_tx, prefill, rich_text_default, account_id);
}

/// A safe file extension for an attachment's temporary copy, so the system's
/// default handler can recognize the file type: the filename's own extension
/// when it's a clean alphanumeric token (≤ 8 chars), else a content-type map,
/// else a `bin` fallback (or the subtype when it's a single clean token, e.g.
/// `application/vnd.foo` → `vnd` is rejected for containing `-`, so it falls
/// through to `bin`).
fn attachment_extension(part: &BodyPart) -> String {
    let from_filename = part
        .filename
        .as_deref()
        .and_then(|f| std::path::Path::new(f).extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|ext| !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    if let Some(ext) = from_filename {
        return ext;
    }
    let subtype = part.content_type.split('/').nth(1).unwrap_or("");
    match part.content_type.as_str() {
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.oasis.opendocument.text" => "odt",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        "application/json" => "json",
        "application/xml" => "xml",
        "application/octet-stream" => "bin",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "message/rfc822" => "eml",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        // Unknown type: the subtype alone when it's a single clean token
        // ("octet-stream" contains a hyphen and correctly lands on `bin`).
        _ if !subtype.is_empty() && subtype.len() <= 8 && subtype.chars().all(|c| c.is_ascii_alphanumeric()) => subtype,
        _ => "bin",
    }
    .to_string()
}

/// The unique temp-file path an "Open" action materializes an attachment at:
/// `$TMPDIR/lookout-<uuid>.<ext>`, where the extension comes from
/// `attachment_extension` so the system's default handler recognizes the
/// type. A fresh uuid per call means concurrent opens never collide.
fn temp_attachment_path(part: &BodyPart) -> PathBuf {
    std::env::temp_dir().join(format!("lookout-{}.{}", uuid::Uuid::new_v4().simple(), attachment_extension(part)))
}

/// Writes the fetched attachment bytes to a unique temporary file and
/// registers it for deletion when Lookout exits. The caller keeps ownership
/// of the path for its own launch step; on a launch failure it should call
/// `discard_temp_attachment` so the file doesn't linger until exit.
fn materialize_temp_attachment(state: &Rc<RefCell<UiState>>, part: &BodyPart, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = temp_attachment_path(part);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    state.borrow_mut().temp_attachment_files.insert(path.clone());
    Ok(path)
}

/// Unregisters and deletes a temp file that will never be opened - a launch
/// failure or an "Open With" dialog dismissed without a choice.
fn discard_temp_attachment(state: &Rc<RefCell<UiState>>, path: &PathBuf) {
    state.borrow_mut().temp_attachment_files.remove(path);
    let _ = std::fs::remove_file(path);
}

/// Opens the fetched attachment bytes with the MIME type's *default*
/// application, from a temporary file. The default app is resolved directly
/// through GIO (`AppInfo::default_for_type`) and launched with
/// `AppInfo::launch` - a plain activation, no chooser, no portal - so a file
/// whose type has a registered default opens in exactly that app. Only when
/// there is no default app for the type (or the direct launch fails, e.g.
/// running sandboxed, where host apps can't be spawned from inside the
/// sandbox) does it fall back to `GtkFileLauncher`, which routes through the
/// XDG portal and lets the portal decide. The temp file is registered in
/// `UiState::temp_attachment_files` and deleted when Lookout exits; a write
/// or launch failure toasts and cleans up immediately.
async fn open_attachment_temp(window: &adw::ApplicationWindow, state: &Rc<RefCell<UiState>>, toast_overlay: adw::ToastOverlay, part: &BodyPart, bytes: &[u8]) {
    let path = match materialize_temp_attachment(state, part, bytes) {
        Ok(path) => path,
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't open attachment: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };
    let file = gio::File::for_path(&path);
    // The default application for the part's MIME type, if one is registered.
    let default_app = gio::content_type_from_mime_type(&part.content_type).and_then(|ct| gio::AppInfo::default_for_type(&ct, false));
    if let Some(app) = default_app {
        match app.launch(std::slice::from_ref(&file), None::<&gio::AppLaunchContext>) {
            Ok(()) => return,
            Err(e) => tracing::warn!("default app {:?} failed to launch attachment: {e}", app.name()),
        }
    }
    // No default app, or it couldn't be launched directly: let the portal
    // take it (it launches the default app, or asks the user when there is
    // none).
    let launcher = gtk::FileLauncher::new(Some(&file));
    if let Err(e) = launcher.launch_future(Some(window)).await {
        discard_temp_attachment(state, &path);
        let title = glib::markup_escape_text(&format!("Couldn't open attachment: {e}"));
        toast_overlay.add_toast(adw::Toast::new(&title));
    }
}

/// Opens the fetched attachment bytes with a user-chosen application, from a
/// temporary file. Asks through the XDG desktop portal's
/// `org.freedesktop.portal.OpenURI` with the `ask` option set - the portal
/// presents its own "Open With" application chooser (the same dialog the
/// portal shows when no default app exists) and launches the picked app on
/// the host, which is what makes this work from inside a sandbox. The temp
/// file is registered in `UiState::temp_attachment_files` and deleted when
/// Lookout exits; a write failure or a portal that isn't reachable toasts and
/// cleans up immediately. The portal's own response (cancelled vs. launched)
/// is deliberately not tracked - the file is simply kept until exit either
/// way, matching the rest of the Open actions.
async fn open_attachment_with(state: &Rc<RefCell<UiState>>, toast_overlay: adw::ToastOverlay, part: &BodyPart, bytes: &[u8]) {
    let path = match materialize_temp_attachment(state, part, bytes) {
        Ok(path) => path,
        Err(e) => {
            let title = glib::markup_escape_text(&format!("Couldn't open attachment: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };

    // The portal's OpenURI method explicitly does not accept `file://` URIs -
    // local files go through OpenFile, which takes the file descriptor. Pass
    // the temp file's fd over the bus with the `ask` option set, so the
    // portal shows its own application chooser and launches the picked app
    // on the host (which is what makes this work from inside a sandbox).
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            discard_temp_attachment(state, &path);
            let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };
    let fd_list = gio::UnixFDList::new();
    let fd_index = match gio::prelude::UnixFDListExtManual::append(&fd_list, &file) {
        Ok(index) => index,
        Err(e) => {
            discard_temp_attachment(state, &path);
            let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };

    // org.freedesktop.portal.OpenURI.OpenFile(parent_window, fd, options):
    // `ask: true` makes the portal present its application chooser instead
    // of silently using the default; `handle_token` names the request (the
    // response signal is deliberately ignored - the temp file is kept until
    // Lookout exits either way). `parent_window` is left empty - a proper
    // surface handle would need XDG foreign-export plumbing per display
    // server, and portals accept an empty one.
    let options = glib::VariantDict::new(None);
    options.insert_value("handle_token", &glib::Variant::from(uuid::Uuid::new_v4().simple().to_string()));
    options.insert_value("ask", &glib::Variant::from(true));
    let options = options.end();
    let args = glib::Variant::tuple_from_iter([glib::Variant::from(""), glib::Variant::from(glib::variant::Handle(fd_index)), options]);

    let proxy = match gio::DBusProxy::for_bus_future(
        gio::BusType::Session,
        gio::DBusProxyFlags::NONE,
        None,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.OpenURI",
    )
    .await
    {
        Ok(proxy) => proxy,
        Err(e) => {
            discard_temp_attachment(state, &path);
            let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
            toast_overlay.add_toast(adw::Toast::new(&title));
            return;
        }
    };
    if let Err(e) = proxy
        .call_with_unix_fd_list_future("OpenFile", Some(&args), gio::DBusCallFlags::NONE, -1, Some(&fd_list))
        .await
    {
        discard_temp_attachment(state, &path);
        let title = glib::markup_escape_text(&format!("Couldn't ask which app to open with: {e}"));
        toast_overlay.add_toast(adw::Toast::new(&title));
    }
}

/// Arms or disarms the "Switch message theme" override's *physical* state on
/// the reading pane's WebView: the user stylesheet that strips the message's
/// backgrounds and inverts its colours, plus the canvas colour (transparent
/// so the app theme shows through while armed, white otherwise, matching
/// WebKit's default). Must be called whenever the logical per-message state
/// (`UiState::message_theme_override`) changes, so the two never drift - the
/// toggle handler, every navigation reset, and the close-pane reset all
/// route through this.
fn set_message_theme_armed(enabled: bool, web_view: &webkit::WebView, user_content_manager: &webkit::UserContentManager, theme_override_sheet: &webkit::UserStyleSheet) {
    if enabled {
        user_content_manager.add_style_sheet(theme_override_sheet);
        web_view.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    } else {
        user_content_manager.remove_style_sheet(theme_override_sheet);
        web_view.set_background_color(&gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0));
    }
}

/// Re-renders the message currently selected in the message list onto the
/// reading pane, routing through "empty" first so `render_body`'s
/// already-shown guard doesn't treat the reload of the same message as a
/// no-op. Used by the Config → Mail "Load images" toggle and the
/// external-content banner's actions, which both change the load policy
/// for the open message and must apply to it rather than the next
/// selection. Skipped while a composer is up - `render_body` would route
/// the pane back to the message page and yank the user out of their draft.
fn rerender_current_message(
    state: &Rc<RefCell<UiState>>,
    reading_stack: &gtk::Stack,
    message_header: &crate::message_header::MessageHeader,
    message_list: &crate::message_list::MessageListModel,
) {
    if reading_stack.visible_child_name().as_deref() == Some("compose") {
        return;
    }
    let (mailbox, uid, body) = {
        let mut st = state.borrow_mut();
        let Some(summary) = message_list.selected_summary() else { return };
        let Some(body) = st.body_cache.get(&summary.mailbox, &summary.uid) else { return };
        // Stash the summary for the fresh render: `render_body` re-derives
        // the reading-pane header and the rendered sender (which the load
        // policy and the trust banner consult) from it.
        let mailbox = summary.mailbox.clone();
        let uid = summary.uid;
        st.pending_header = Some(summary);
        (mailbox, uid, body)
    };
    // Drop `rendered_message` and route through "empty" first, or
    // `render_body`'s already-shown guard would treat the reload of the
    // same message as a no-op and never re-issue the `load_html`.
    reading_stack.set_visible_child_name("empty");
    state.borrow_mut().rendered_message = None;
    render_body(state, reading_stack, message_header, mailbox, uid, body);
}

/// Strips the angle brackets an `In-Reply-To`/`References` value must not
/// carry (the composer prefill's convention: `mail_builder`'s MessageId
/// writer adds them itself).
fn bare_message_id(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(trimmed).to_string()
}

/// Sends the RFC 8098 read receipt for the message currently on the reading
/// pane - the `Disposition-Notification-To` request and the original-message
/// context stashed by `render_body`. `automatic` picks the report's
/// disposition mode (`automatic-action/MDN-sent-automatically` vs
/// `manual-action/MDN-sent-manually`). Returns whether the receipt was
/// queued; on success the message is marked receipted (the banner won't
/// offer it again) and a toast confirms. Both callers - the banner's Send
/// button and the automatic policy - route through this, so the receipt is
/// built and sent one way. The send itself is the same `SendMessage` path
/// the composer uses (SMTP + a copy in Sent), from the receiving account's
/// own address so the receipt is legitimate.
fn send_read_receipt(state: &Rc<RefCell<UiState>>, automatic: bool) -> bool {
    let (mailbox, uid, request, context) = {
        let st = state.borrow();
        let (mailbox, uid) = match &st.rendered_message {
            Some(rendered) => rendered.clone(),
            None => return false,
        };
        let Some(request) = st.read_receipt_request.clone() else { return false };
        let Some(context) = st.read_receipt_context.clone() else { return false };
        (mailbox, uid, request, context)
    };
    let Some(account_id) = mailbox_account_id(&mailbox) else { return false };
    let account_lookup = {
        let st = state.borrow();
        st.accounts
            .get(&account_id)
            .map(|handle| (handle.email.clone(), handle.display_name.clone(), handle.cmd_tx.clone()))
    };
    let Some((email, display_name, cmd_tx)) = account_lookup else {
        return false;
    };
    let Some(to) = request.first() else { return false };
    let original_message_id = context.message_id;
    let bare_id = bare_message_id(&original_message_id);
    let subject = if context.subject.trim().is_empty() {
        "Read receipt".to_string()
    } else {
        format!("Read receipt: {}", context.subject.trim())
    };
    let message = lookout_mail::ComposedMessage {
        from: email.clone(),
        display_name: Some(display_name).filter(|name| !name.trim().is_empty()),
        to: vec![to.clone()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject,
        // The report branch of `build_raw_message` replaces the text body
        // with the human-readable part it builds itself.
        text_body: String::new(),
        html_body: None,
        attachments: vec![],
        inline_images: vec![],
        calendar_part: None,
        read_receipt: Some(lookout_mail::ReadReceipt {
            original_message_id,
            original_from: context.original_from,
            original_subject: context.subject,
            original_date: context.date,
            final_recipient: email,
            automatic,
            displayed_at: chrono::Local::now().format("%a, %e %b %Y %H:%M %z").to_string(),
            original_headers: context.headers,
        }),
        request_read_receipt: false,
        in_reply_to: Some(bare_id.clone()),
        references: vec![bare_id],
        message_id: None,
    };
    if cmd_tx.send_blocking(AccountCommand::SendMessage(Box::new(message))).is_err() {
        return false;
    }
    {
        let mut st = state.borrow_mut();
        st.read_receipts_sent.insert((mailbox.clone(), uid));
        st.read_receipt_dismissed = Some((mailbox, uid));
        if let Some(toast_overlay) = &st.toast_overlay {
            toast_overlay.add_toast(adw::Toast::new("Read receipt sent"));
        }
    }
    true
}

fn render_body(
    state: &Rc<RefCell<UiState>>,
    reading_stack: &gtk::Stack,
    message_header: &crate::message_header::MessageHeader,
    mailbox: MailboxId,
    uid: Uid,
    body: lookout_core::EmailBody,
) {
    // Defense-in-depth for the direct `BodyFetched` caller: if this exact
    // message is already on screen, a duplicate body event must not route the
    // pane through "empty" and crossfade the same email again. The selection
    // handler's `already_shown` guard covers the normal path; this covers a
    // stray re-fetch racing a repopulate. (The selection handler's cached
    // path passes through "empty" before calling this, so a fresh render of
    // the newly-selected message still lands here.)
    let already_shown = {
        let st = state.borrow();
        st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid)) && reading_stack.visible_child_name().as_deref() == Some("message")
    };
    if already_shown {
        return;
    }
    // Remember what's being rendered so a later re-selection of the same
    // message (see the selection handler's `already_shown` guard) can be
    // recognized as a no-op instead of another crossfade.
    state.borrow_mut().rendered_message = Some((mailbox.clone(), uid));
    // The trust banner's `html_remote_content_scan` re-parses the message's
    // whole HTML, which is unchanged across re-renders of the same message
    // (theme toggle, list repopulate keeping the selection). Compute it once
    // per render into a single-slot cache keyed by this message; the banner
    // block reads the cache instead of rescanning.
    {
        let mut st = state.borrow_mut();
        let cached = st.rendered_remote_scan.as_ref().filter(|(m, u, _)| m == &mailbox && u == &uid).map(|(_, _, s)| *s);
        st.rendered_remote_scan = Some((
            mailbox.clone(),
            uid,
            cached.unwrap_or_else(|| body.html_body.as_deref().map(lookout_core::html_remote_content_scan).unwrap_or_default()),
        ));
    }
    // The pane is about to show a new message: any inline `cid:` image
    // requests still in flight belong to the one being replaced, so finish
    // them with an error (WebKit re-requests if the same images come up
    // again, and the cache serves those). Then stash the new message's
    // `cid:`-bearing parts for the scheme handler to resolve against.
    drop_pending_cid(state);
    state.borrow_mut().rendered_inline_parts = body.parts.iter().filter(|p| p.cid.is_some()).cloned().collect();
    // Apply the header for the message being rendered now. The selection
    // handler stores the summary here instead of updating the header
    // immediately, so the previous message's header stays on screen through
    // its whole fade-out; by the time we get here the pane is already on the
    // "empty" placeholder, so updating the (hidden) header can't flash. The
    // summary's From address is also stashed - normalized - as the rendered
    // sender the load policy resolves against `trusted_senders` and the
    // trust banner shows. The debug `.eml` viewer has no summary (and no
    // account to key trust on anyway), so the sender ends up `None` there
    // and all remote content stays blocked.
    {
        let mut st = state.borrow_mut();
        if let Some(summary) = st.pending_header.take() {
            message_header.update(&summary);
            let sender = summary.from.first().map(|a| a.address.trim().to_lowercase());
            st.rendered_trust_sender = mailbox_account_id(&mailbox).zip(sender);
        } else {
            st.rendered_trust_sender = None;
        }
    }
    // Sync the "Switch message theme" toggle with its per-email state: the
    // selection handler resets `message_theme_override` to the configured
    // default on every navigation (with the physical sheet/canvas already
    // re-armed to match), so this flips the header button accordingly for
    // the next message. A no-op `set_active` (same value) emits no `toggled`
    // signal, so this can't feed back into the toggle's handler or its
    // re-render.
    message_header.theme_button.set_active(state.borrow().message_theme_override);
    // Rebuild the attachment strip from the body's part list; the body is
    // available regardless of which text path (html/text/none) renders below.
    rebuild_attachment_strip(state, reading_stack, &mailbox, uid, &body.parts);
    // List-Unsubscribe banner: stash the rendered message's parsed
    // unsubscribe actions for the banner's button handler, and reveal the
    // banner when the message offers an action the user hasn't dismissed for
    // this message (the close button records the dismissal). Hidden
    // otherwise - and on every navigation, since the selection handler
    // clears `unsubscribe_dismissed`.
    {
        let mut st = state.borrow_mut();
        st.unsubscribe_info = lookout_core::parse_list_unsubscribe(&body.headers);
        let dismissed = st.unsubscribe_dismissed.as_ref() == Some(&(mailbox.clone(), uid));
        if let Some(banner) = find_named_child(reading_stack, "unsubscribe-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            banner.set_revealed(!dismissed && st.unsubscribe_info.is_some());
        }
    }
    // iMIP banner: parse the rendered message's `text/calendar` payload (if
    // any) into the invitation the banner's button handler acts on, and
    // reveal the banner when there's one the user hasn't dismissed for this
    // message. The per-method button label/title is set here too - it varies
    // with what the payload asks of the user. Hidden otherwise - and on every
    // navigation, since the selection handler clears `imip_dismissed`.
    {
        let mut st = state.borrow_mut();
        st.imip = body.calendar_ics.as_deref().and_then(lookout_dav::parse_imip_invitation);
        if let Some(invitation) = st.imip.as_mut() {
            // The reply's In-Reply-To is the invitation message's own
            // Message-ID, which lives in the message headers rather than the
            // iCalendar document.
            invitation.in_reply_to = lookout_core::header_value(&body.headers, "message-id").map(str::to_string);
        }
        let dismissed = st.imip_dismissed.as_ref() == Some(&(mailbox.clone(), uid));
        if let Some(banner) = find_named_child(reading_stack, "imip-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            let (title, button) = match (&st.imip, dismissed) {
                (Some(invitation), false) => match invitation.method {
                    lookout_core::ImipMethod::Request => (format!("Invitation: {}", invitation.summary.as_deref().unwrap_or("an event")), "Respond…".to_string()),
                    lookout_core::ImipMethod::Cancel => (
                        format!("Cancelled: {}", invitation.summary.as_deref().unwrap_or("an event")),
                        "Remove from calendar".to_string(),
                    ),
                    lookout_core::ImipMethod::Reply => (format!("RSVP update: {}", invitation.summary.as_deref().unwrap_or("an event")), "Dismiss".to_string()),
                },
                _ => (String::new(), String::new()),
            };
            banner.set_title(&title);
            banner.set_button_label(Some(&button));
            banner.set_revealed(!dismissed && st.imip.is_some());
            // The invite-details card mirrors the banner's visibility: shown
            // for the same payload, hidden once the user dismisses it (or
            // when the message carries no invitation at all - `st.imip` was
            // set from this message's `text/calendar` part above).
            render_invite_card(reading_stack, st.imip.as_ref().filter(|_| !dismissed));
        }
    }
    // External-content trust banner: reveal it when the message on screen
    // references remote content the load policy is *currently blocking* -
    // its sender isn't trusted (at a high enough level) and the global
    // "Load images from the web" toggle isn't covering the gap. The HTML
    // scan is advisory; the decide-policy handler stays authoritative. The
    // sender comes from `rendered_trust_sender`, stashed above; hidden
    // when there's no sender to key trust on (the debug `.eml` viewer).
    {
        let st = state.borrow();
        let dismissed = st.trust_banner_dismissed.as_ref() == Some(&(mailbox.clone(), uid));
        if let Some(banner) = find_named_child(reading_stack, "trust-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            match st.rendered_trust_sender.clone() {
                // No sender to key trust on (the debug `.eml` viewer): the
                // banner has nothing to act on, so leave it hidden.
                None => banner.set_revealed(false),
                Some((account, sender)) => {
                    let level = st.trusted_senders.get(&(account, sender.clone())).copied();
                    // `html_remote_content_scan` re-parses the whole rendered
                    // HTML; `render_body` computed it once per render and
                    // cached it keyed by this message (see below), so a
                    // re-render of the same message reuses the scan instead of
                    // rescanning.
                    let scan = st
                        .rendered_remote_scan
                        .as_ref()
                        .filter(|(m, u, _)| m == &mailbox && u == &uid)
                        .map(|(_, _, s)| *s)
                        .unwrap_or_default();
                    let images_blocked = scan.has_images && !(st.load_remote_images || st.load_once_images || level.is_some_and(|l| l >= lookout_core::TrustLevel::Images));
                    let other_blocked = scan.has_other && !level.is_some_and(|l| l >= lookout_core::TrustLevel::AllContent);
                    banner.set_title(&format!("Remote content from {sender} is blocked"));
                    banner.set_revealed(!dismissed && (images_blocked || other_blocked));
                }
            }
        }
    }
    // Read-receipt request (RFC 8098 `Disposition-Notification-To`): stash
    // the rendered message's requested addresses - and the original-message
    // context a receipt needs - for the banner's button handler and the
    // automatic policy. Under the automatic policy the receipt goes out
    // immediately (once per message per session) with a toast instead of a
    // banner; otherwise the banner asks. Neither happens for
    // machine-generated mail (RFC 3834) or reports (RFC 8098 §2.1.4) - a
    // receipt for a receipt is how loops start - and never for the debug
    // `.eml` viewer, which has no account to send from.
    {
        let request = lookout_core::parse_disposition_notification_to(&body.headers);
        let mut st = state.borrow_mut();
        st.read_receipt_request = (!request.is_empty()).then_some(request);
        st.read_receipt_context = st.read_receipt_request.as_ref().map(|_| ReadReceiptContext {
            message_id: lookout_core::header_value(&body.headers, "message-id").unwrap_or("").to_string(),
            original_from: lookout_core::header_value(&body.headers, "from").unwrap_or("").to_string(),
            subject: lookout_core::header_value(&body.headers, "subject").unwrap_or("").to_string(),
            date: lookout_core::header_value(&body.headers, "date").map(str::to_string),
            headers: body.headers.clone(),
        });
        let key = (mailbox.clone(), uid);
        let automatic = st.settings.get_bool(crate::settings::MAIL_SEND_READ_RECEIPTS);
        let eligible = st.read_receipt_request.is_some()
            && !lookout_core::is_auto_submitted(&body.headers)
            && !lookout_core::is_report_message(&body.headers)
            && mailbox_account_id(&mailbox).is_some();
        let already_sent = st.read_receipts_sent.contains(&key);
        if let Some(banner) = find_named_child(reading_stack, "read-receipt-banner").and_then(|child| child.downcast::<adw::Banner>().ok()) {
            banner.set_revealed(eligible && !automatic && !already_sent && st.read_receipt_dismissed.as_ref() != Some(&key));
        }
        drop(st);
        if automatic && eligible && !already_sent {
            send_read_receipt(state, true);
        }
    }
    // Config → Appearance → "Animate transitions" can switch the stack's
    // transition type to `None`; when it's off, skip the fade-specific paths
    // below (routing through "empty", waiting for the WebView to paint) and
    // swap content in directly, matching the pre-fade behavior.
    let animated = reading_stack.transition_type() != gtk::StackTransitionType::None;
    // The "message" page groups the header with the body's content stack
    // (web view / text view), so revealing it - vs. the "empty" page - is
    // what crossfades, carrying the whole header + body together.
    let Some(content_stack) = find_named_child(reading_stack, "body").and_then(|child| child.downcast::<gtk::Stack>().ok()) else {
        return;
    };
    if let Some(html) = &body.html_body {
        if let Some(web_view) = content_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
            if !animated {
                web_view.load_html(html, None);
                content_stack.set_visible_child_name("html");
                reading_stack.set_visible_child_name("message");
                return;
            }
            // Re-render of the same page: GTK only transitions when the
            // visible child actually changes, so drop back to "empty" first
            // and let the reveal crossfade the fresh body in.
            if reading_stack.visible_child_name().as_deref() == Some("message") {
                reading_stack.set_visible_child_name("empty");
            }
            // WebKit paints asynchronously - crossfading into the HTML page
            // while a fresh body is still loading would show a blank/white
            // page before the message appears. So arm the persistent
            // `load-changed` handler (`pending_html_reveal`) and let it
            // reveal the page once the load completes. The selection handler
            // disarms it on every selection change, so a load started for a
            // message the user has already moved on from can never reveal.
            state.borrow_mut().pending_html_reveal = true;
            web_view.load_html(html, None);
            tracing::debug!(?mailbox, uid = uid.0, "render_body: load_html issued");
            // The reveal above is gated on WebKit's `Finished` event, but a
            // slow/hung load must not be able to hold the pane on "empty"
            // indefinitely. Arm a fallback that reveals the page after a
            // short grace period if the load hasn't finished. It captures
            // `reveal_generation` at arm time and only reveals if the counter
            // is unchanged, so a stale timeout from a message the user has
            // already left can't pop the next message's page open mid-load
            // (the selection handler bumps the counter when it disarms
            // `pending_html_reveal`).
            let generation = state.borrow().reveal_generation;
            let state_for_timeout = state.clone();
            let reading_stack_for_timeout = reading_stack.clone();
            let content_stack_for_timeout = content_stack.clone();
            let mailbox_for_timeout = mailbox.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(HTML_REVEAL_TIMEOUT_MS), move || {
                let mut st = state_for_timeout.borrow_mut();
                let still_armed = st.pending_html_reveal && st.reveal_generation == generation;
                // Backstop for the same scenario the `load-changed` handler
                // guards against: `pending_html_reveal`/`reveal_generation`
                // disarmed by something other than a genuine navigation away
                // from this message, stranding the pane on "empty" with
                // nothing left to reveal it. If `rendered_message` still
                // names this exact message, revealing now is still correct.
                let stuck_on_empty = st.rendered_message.as_ref() == Some(&(mailbox_for_timeout.clone(), uid))
                    && reading_stack_for_timeout.visible_child_name().as_deref() == Some("empty");
                if still_armed || stuck_on_empty {
                    st.pending_html_reveal = false;
                    drop(st);
                    reveal_message_page(&reading_stack_for_timeout, &content_stack_for_timeout, "html");
                }
            });
            return;
        }
    }
    if let Some(text) = &body.text_body {
        if let Some(scroller) = content_stack.child_by_name("text").and_downcast::<gtk::ScrolledWindow>() {
            if let Some(text_view) = scroller.child().and_downcast::<gtk::TextView>() {
                text_view.buffer().set_text(text);
                if animated && reading_stack.visible_child_name().as_deref() == Some("message") {
                    // Same-page re-render: route through "empty", wait out
                    // the fade-out, then reveal so both halves of the
                    // transition are visible.
                    let reading_stack_for_reveal = reading_stack.clone();
                    let content_stack_for_reveal = content_stack.clone();
                    let duration = reading_stack.transition_duration() as u64;
                    reading_stack.set_visible_child_name("empty");
                    glib::timeout_add_local_once(std::time::Duration::from_millis(duration), move || {
                        reveal_message_page(&reading_stack_for_reveal, &content_stack_for_reveal, "text");
                    });
                } else {
                    reveal_message_page(reading_stack, &content_stack, "text");
                }
                return;
            }
        }
    }
    reading_stack.set_visible_child_name("empty");
    state.borrow_mut().rendered_message = None;
}

/// Swaps a compose widget into the reading pane's `"compose"` stack page,
/// replacing whatever was showing there (a message, or the empty
/// placeholder). Removes any leftover `"compose"` page first so repeated
/// clicks don't accumulate stale pages, and restores whatever page was
/// visible beforehand once `on_done` fires (Cancel or Send) - so Reply's
/// Cancel lands back on the same message, and New Message's Cancel lands
/// back on the empty placeholder. Arms the composer's header pop-out button:
/// clicking it moves the still-alive composer into its own window (see the
/// `on_pop_out` closure below), and closing that window pops the composer
/// back into this stack - the same move-in / move-out round trip as the
/// People screen's detach.
#[allow(clippy::too_many_arguments)]
fn show_composer_in_reading_pane(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    reading_stack: &gtk::Stack,
    title: &str,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill: crate::compose::ComposePrefill,
    rich_text_default: bool,
    account_id: Option<AccountId>,
) {
    if let Some(existing) = reading_stack.child_by_name("compose") {
        reading_stack.remove(&existing);
    }
    let previous_page = reading_stack.visible_child_name().map(|s| s.to_string()).unwrap_or_else(|| "empty".to_string());
    let reading_stack_for_close = reading_stack.clone();
    let state_for_close = state.clone();
    // The pop-out closure below shares these with `on_done` - clone them
    // before `on_done` moves the originals in.
    let reading_stack_for_popout = reading_stack.clone();
    let state_for_popout = state.clone();
    let previous_page_for_popout = previous_page.clone();
    // Claim the draft-confirmation relay / identities-refresh slots under a
    // fresh generation. A finishing composer only clears them when its own
    // generation still owns them, so a popped-out composer that finishes
    // after a newer inline composer opened can't strip the newer one's
    // relays.
    let relay_generation = {
        let mut st = state.borrow_mut();
        st.composer_relay_generation += 1;
        st.composer_relay_generation
    };
    // True while this composer lives in its pop-out window. Set by the
    // pop-out handler, cleared by the window's close handler on pop-back-in.
    // `on_done` consults it to skip the reading-pane surgery entirely while
    // popped out: the compose page was already removed at pop-out (and any
    // page under that name now belongs to a newer composer), and there's no
    // pre-compose page to restore.
    let popped = Rc::new(Cell::new(false));
    let popped_for_popout = popped.clone();
    let on_done: Rc<dyn Fn()> = Rc::new(move || {
        if !popped.get() {
            // Switch away from the composer's page before removing it, not
            // after: the stack's crossfade transition snapshots the
            // outgoing page to blend it out, and removing "compose" first
            // (only then switching) tore the widget out from under that
            // snapshot, leaving a stale ghost of its card-styled fields
            // group painted over the background once the transition
            // finished. Restore the pre-compose page only if the pane is
            // still on the composer.
            if reading_stack_for_close.visible_child_name().as_deref() == Some("compose") {
                reading_stack_for_close.set_visible_child_name(&previous_page);
            }
            if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
                reading_stack_for_close.remove(&existing);
            }
        }
        let mut st = state_for_close.borrow_mut();
        if st.composer_relay_generation == relay_generation {
            // The composer is gone; drop its draft-confirmation relay so its
            // consumer future exits and late events go nowhere, and its
            // identities-refresh hook so a later Config manage-identities
            // edit doesn't poke a dead dropdown.
            st.draft_saved_tx = None;
            st.composer_identities_refresh = None;
        }
        // Close this composer's own pop-out window when it finishes -
        // Cancel/Send in the window must take it down, not leave a dead
        // composer on screen. Guarded by the window's own generation, which
        // is independent of the relay ownership above: a popped-out composer
        // finishing after a newer inline composer opened still closes its
        // own window.
        let owns_window = st.compose_popout_window.as_ref().map(|(gen, _)| *gen == relay_generation).unwrap_or(false);
        if owns_window {
            if let Some((_, win)) = st.compose_popout_window.take() {
                win.destroy();
            }
        }
    });
    // The header's pop-out button hands the composer to this closure, which
    // moves it into its own window - the composer itself stays alive (draft
    // autosave included), so nothing needs rebuilding or re-seeding. The
    // composer set `moving`/`popped_out` around the removal, which the
    // autosave's `root-notify` guard consults so the move isn't mistaken for
    // a displacement. Closing the window pops the composer back into the
    // reading pane (same `"compose"` page, displacing any newer composer
    // that opened meanwhile), unless Send/Cancel already finished the
    // session - then the window just closes.
    let on_pop_out: Option<Rc<dyn Fn(crate::compose::PopOutHandle)>> = Some(Rc::new({
        let state_for_close = state_for_popout;
        let reading_stack_for_close = reading_stack_for_popout;
        let previous_page = previous_page_for_popout;
        let title = title.to_string();
        let popped = popped_for_popout;
        move |handle| {
            popped.set(true);
            // Switch away from "compose" before removing it - see the
            // matching note in `on_done` above for why the order matters
            // (a stale crossfade snapshot ghosting over the background).
            if reading_stack_for_close.visible_child_name().as_deref() == Some("compose") {
                reading_stack_for_close.set_visible_child_name(&previous_page);
            }
            if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
                reading_stack_for_close.remove(&existing);
            }
            // A header bar of its own is what gives the window a drag
            // region - a bare `adw::Window` has none (the People pop-out
            // needed the same, see 0.8.0).
            let win = adw::Window::builder().default_width(860).default_height(640).build();
            let header = adw::HeaderBar::new();
            header.set_title_widget(Some(&adw::WindowTitle::new(&title, "")));
            // Cancel, Send, From and the read-receipt toggle live in the
            // title bar while popped out - the composer's own header row is
            // redundant once the window has a title bar (and is hidden
            // wholesale, see compose.rs's `root-notify` handler) - and are
            // moved back on pop-back-in (see the close handler below).
            // They're the same widgets, so their handlers keep working
            // across the move. The draft status label moves into the title
            // bar with them; the title label stays hidden with its row.
            handle.top_row.remove(&handle.cancel_button);
            handle.top_row.remove(&handle.send_button);
            handle.top_row.remove(&handle.status_label);
            handle.top_row.remove(&handle.from_dropdown);
            handle.top_row.remove(&handle.read_receipt_toggle);
            // A button to pop the composer back into the reading pane
            // without closing the session: it closes the window, and the
            // close handler below performs the pop-back-in (the same path
            // as the window's own close button, minus the "already
            // finished" branch).
            let back_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popin1", "go-previous-symbolic", "go-back-symbolic"]));
            back_button.set_tooltip_text(Some("Back to main window"));
            {
                let win = win.clone();
                back_button.connect_clicked(move |_| win.close());
            }
            // Header bar packing order: start-side children are prepended
            // (each new call lands leftmost), end-side children are appended
            // outward (each new call lands further right), so this yields,
            // left to right: [from, send | title | status, cancel, back,
            // read receipt] - the read-receipt toggle is now the outermost
            // widget on the right.
            header.pack_start(&handle.send_button);
            header.pack_start(&handle.from_dropdown);
            header.pack_end(&handle.status_label);
            header.pack_end(&handle.cancel_button);
            header.pack_end(&back_button);
            header.pack_end(&handle.read_receipt_toggle);
            let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
            content_box.append(&header);
            content_box.append(&handle.widget);
            win.set_content(Some(&content_box));
            // Held here (not just by the closure below) so the window
            // survives after this handler returns - GTK only keeps a
            // presented window alive while it's on screen.
            state_for_close.borrow_mut().compose_popout_window = Some((relay_generation, win.clone()));
            {
                let state_for_close = state_for_close.clone();
                let reading_stack_for_close = reading_stack_for_close.clone();
                let popped = popped.clone();
                let handle = handle;
                win.connect_close_request(move |_| {
                    if !handle.done.get() {
                        handle.moving.set(true);
                        handle.popped_out.set(false);
                        if let Some(parent) = handle.widget.parent().and_then(|parent| parent.downcast::<gtk::Box>().ok()) {
                            parent.remove(&handle.widget);
                        }
                        // Send Cancel, Send, From and the read-receipt
                        // toggle home to the composer's header row, out of
                        // the title bar they're about to leave. `pack_start`/
                        // `pack_end` parent a widget to an internal box
                        // inside `AdwHeaderBar`, not the header bar itself,
                        // so a type-checked removal off the header bar never
                        // matches - `unparent()` detaches a widget from
                        // whatever its actual current parent is.
                        handle.cancel_button.unparent();
                        handle.send_button.unparent();
                        handle.status_label.unparent();
                        handle.from_dropdown.unparent();
                        handle.read_receipt_toggle.unparent();
                        handle.top_row.prepend(&handle.from_dropdown);
                        handle.top_row.prepend(&handle.send_button);
                        handle.top_row.append(&handle.cancel_button);
                        handle.top_row.append(&handle.status_label);
                        handle.top_row.reorder_child_after(&handle.status_label, Some(&handle.title_label));
                        handle.top_row.append(&handle.read_receipt_toggle);
                        handle.top_row.reorder_child_after(&handle.read_receipt_toggle, Some(&handle.status_label));
                        if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
                            reading_stack_for_close.remove(&existing);
                        }
                        reading_stack_for_close.add_named(&handle.widget, Some("compose"));
                        handle.moving.set(false);
                        popped.set(false);
                        reading_stack_for_close.set_visible_child_name("compose");
                    }
                    state_for_close.borrow_mut().compose_popout_window = None;
                    glib::Propagation::Proceed
                });
            }
            win.present();
        }
    }));
    // Recipient autocomplete combines local mail-history addresses with
    // CardDAV contacts discovered for this account. CardDAV is queried from
    // UI memory synchronously (cheap, no SQLite); the mail-history half is
    // an off-thread cache read (`spawn_cache_read`) - the completion callback
    // fires once it answers, merged with the CardDAV half computed up front.
    let address_cache = account_id.clone().and_then(|id| state.borrow().accounts.get(&id).and_then(|h| h.address_cache.clone()));
    let carddav_provider = SnapshotContactsProvider {
        state: state.clone(),
        account: account_id.clone(),
    };
    let worker_for_suggestions = worker.clone();
    let suggestions: crate::recipient_entry::SuggestionSource = Rc::new(move |prefix: &str, complete: Box<dyn FnOnce(Vec<lookout_core::EmailAddress>)>| {
        let carddav = carddav_provider.search_contacts(prefix, 8);
        let Some(cache) = address_cache.clone() else {
            complete(merge_contact_suggestions(Vec::new(), &carddav, prefix.trim(), 8));
            return;
        };
        let reply_rx = spawn_cache_read(&worker_for_suggestions, cache, {
            let prefix = prefix.to_string();
            move |cache| cache.search_contacts(&prefix, 8)
        });
        let prefix = prefix.trim().to_string();
        glib::spawn_future_local(async move {
            let mail_history = reply_rx.recv().await.unwrap_or_default();
            complete(merge_contact_suggestions(mail_history, &carddav, &prefix, 8));
        });
    });
    // Shows the persistent "Sending: <subject>" toast the instant Send is
    // clicked, and registers it in `sending_toasts` so the account event
    // loop can retract it once the send actually completes or fails - see
    // `AccountEvent::SendCompleted`/`SendFailed` in `connect_account`.
    let on_send_started: Rc<dyn Fn(String)> = {
        let state = state.clone();
        let account_id = account_id.clone();
        Rc::new(move |subject: String| {
            let toast = adw::Toast::new(&format!("Sending: {subject}"));
            toast.set_timeout(0);
            let overlay = state.borrow().toast_overlay.clone();
            if let Some(overlay) = overlay {
                overlay.add_toast(toast.clone());
            }
            if let Some(id) = &account_id {
                state.borrow_mut().sending_toasts.entry(id.clone()).or_default().push_back(toast);
            }
        })
    };
    let (composer, draft_tx, identities_refresh) = crate::compose::build_compose_view(
        title,
        // The composer's From dropdown re-reads from here whenever the
        // Config → Mail accounts manage-identities dialog fires `on_changed`
        // (see `composer_identities_refresh`), so edits made while a
        // composer is open are live. Falls back to the first connected
        // account when no explicit account was resolved.
        {
            let state = state.clone();
            let account_id = account_id.clone();
            Rc::new(move || {
                let st = state.borrow();
                let resolved = account_id.clone().or_else(|| st.accounts.keys().next().cloned());
                match resolved {
                    Some(id) => match st.accounts.get(&id) {
                        Some(handle) => st.app_config.borrow().identities_for_account(&id, &handle.display_name, &handle.email),
                        None => Vec::new(),
                    },
                    None => Vec::new(),
                }
            })
        },
        cmd_tx,
        prefill,
        on_done,
        rich_text_default,
        suggestions,
        on_pop_out,
        on_send_started,
    );
    // Replacing any previous composer's relay (dropped sender = its consumer
    // exits), and hooking the Config manage-identities dialog into the new
    // composer's From dropdown.
    state.borrow_mut().draft_saved_tx = Some(draft_tx);
    state.borrow_mut().composer_identities_refresh = Some(identities_refresh);
    reading_stack.add_named(&composer, Some("compose"));
    reading_stack.set_visible_child_name("compose");
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::UidValidity;

    fn test_mailbox(account: &AccountId, name: &str, unread: u32) -> Mailbox {
        Mailbox {
            id: MailboxId::new(account, name),
            account_id: account.clone(),
            name: name.to_string(),
            parent: None,
            delimiter: '/',
            role: MailboxRole::Inbox,
            uidvalidity: UidValidity(1),
            uidnext: 1,
            highest_modseq: None,
            total: 0,
            unread,
            flags: vec![],
            subscribed: true,
        }
    }

    #[test]
    fn folder_tree_signature_tracks_the_unread_count() {
        let account = AccountId("acc".into());
        let before = folder_tree_signature(&[(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 3)])], &[]);
        let after = folder_tree_signature(&[(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 4)])], &[]);
        assert_ne!(before, after, "a changed count must rebuild the tree - it's what the row draws");
    }

    #[test]
    fn goa_accounts_are_enabled_by_default_and_toggle_persists() {
        let state = test_state(Vec::new());
        let id = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".into());
        let other = AccountId("/org/gnome/OnlineAccounts/Accounts/account_2".into());
        assert!(state.borrow().account_enabled(&id), "accounts are enabled by default");
        state.borrow().set_account_enabled(&id, false);
        assert!(!state.borrow().account_enabled(&id));
        // Disabling one account leaves the others enabled.
        assert!(state.borrow().account_enabled(&other));
        // Re-enabling works.
        state.borrow().set_account_enabled(&id, true);
        assert!(state.borrow().account_enabled(&id));
    }

    fn attachment(part_number: &str, filename: Option<&str>) -> BodyPart {
        BodyPart {
            part_number: part_number.to_string(),
            content_type: "application/pdf".to_string(),
            charset: None,
            transfer_encoding: Some("base64".to_string()),
            filename: filename.map(str::to_string),
            cid: None,
            size: 0,
            is_attachment: true,
        }
    }

    #[test]
    fn human_size_uses_binary_units_and_degrades_to_bytes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(12 * 1024 + 512), "12.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(human_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn attachment_display_name_prefers_filename_and_sanitizes_placeholders() {
        assert_eq!(attachment_display_name(&attachment("2", Some("report.pdf"))), "report.pdf");
        // A filename that could smuggle a path separator must not reach the
        // save dialog as a path - fall back to the part-number placeholder.
        assert_eq!(attachment_display_name(&attachment("2", Some("../etc/passwd"))), "attachment-2");
        assert_eq!(attachment_display_name(&attachment("2", Some(""))), "attachment-2");
        assert_eq!(attachment_display_name(&attachment("2", None)), "attachment-2");
        assert_eq!(attachment_display_name(&attachment("1.3", None)), "attachment-1.3");
    }

    /// The .eml export dialog's suggested name comes from the message's
    /// subject, with a path-free placeholder for empty or hostile subjects -
    /// the raw subject is untrusted header data and must not reach the dialog
    /// as a path.
    #[test]
    fn eml_suggested_name_uses_subject_and_falls_back_safely() {
        let with_subject = |subject: Option<&str>| EmailSummary {
            subject: subject.map(str::to_string),
            ..summary(Uid(1), "INBOX", 2026, 7, 30, 12)
        };
        assert_eq!(eml_suggested_name(&with_subject(Some("Quarterly report"))), "Quarterly report.eml");
        // Whitespace-only and missing subjects fall back to the placeholder.
        assert_eq!(eml_suggested_name(&with_subject(Some("  "))), "message.eml");
        assert_eq!(eml_suggested_name(&with_subject(None)), "message.eml");
        // A subject that could smuggle a path separator must not reach the
        // save dialog as a path.
        assert_eq!(eml_suggested_name(&with_subject(Some("../etc/passwd"))), "message.eml");
        assert_eq!(eml_suggested_name(&with_subject(Some("a\\b"))), "message.eml");
    }

    fn attachment_with_type(part_number: &str, filename: Option<&str>, content_type: &str) -> BodyPart {
        BodyPart {
            part_number: part_number.to_string(),
            content_type: content_type.to_string(),
            charset: None,
            transfer_encoding: Some("base64".to_string()),
            filename: filename.map(str::to_string),
            cid: None,
            size: 0,
            is_attachment: true,
        }
    }

    /// The temp-file copy's extension decides which handler opens it, so the
    /// filename's own extension wins when clean, and content-type mappings
    /// cover the no-name and hostile-name cases.
    #[test]
    fn attachment_extension_prefers_clean_filename_extensions() {
        assert_eq!(attachment_extension(&attachment_with_type("2", Some("report.pdf"), "application/pdf")), "pdf");
        // Lowercased - handlers expect conventional extensions.
        assert_eq!(attachment_extension(&attachment_with_type("2", Some("Photo.PNG"), "image/png")), "png");
        // A hostile or degenerate filename's extension is ignored, and the
        // content type answers instead.
        assert_eq!(attachment_extension(&attachment_with_type("2", Some("report.<script>"), "application/pdf")), "pdf");
        assert_eq!(attachment_extension(&attachment_with_type("2", Some("report."), "application/pdf")), "pdf");
        assert_eq!(attachment_extension(&attachment_with_type("2", Some("noext"), "application/pdf")), "pdf");
    }

    #[test]
    fn attachment_extension_falls_back_through_content_type_to_bin() {
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/pdf")), "pdf");
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "image/jpeg")), "jpg");
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "message/rfc822")), "eml");
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "text/csv")), "csv");
        assert_eq!(
            attachment_extension(&attachment_with_type("2", None, "application/vnd.openxmlformats-officedocument.wordprocessingml.document")),
            "docx"
        );
        // Unknown types: a single clean subtype token is kept, anything with
        // punctuation or over-length falls back to `bin`.
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/xfoobar")), "xfoobar");
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/octet-stream")), "bin");
        assert_eq!(attachment_extension(&attachment_with_type("2", None, "application/vnd.example-thing")), "bin");
    }

    /// The temp-file name must be unique per call, carry the attachment's
    /// extension (so the handler recognizes it), and never smuggle path
    /// separators into the file name.
    #[test]
    fn temp_attachment_path_is_unique_named_and_sanitized() {
        let part = attachment_with_type("2", Some("report.pdf"), "application/pdf");
        let a = temp_attachment_path(&part);
        let b = temp_attachment_path(&part);
        assert_ne!(a, b, "each open materializes its own copy");
        assert_eq!(a.parent(), std::env::temp_dir().as_path().into());
        let name = a.file_name().and_then(|n| n.to_str()).expect("utf-8 temp name");
        assert!(name.starts_with("lookout-"), "identifiable as Lookout's: {name}");
        assert!(name.ends_with(".pdf"), "the handler's extension must survive: {name}");
        assert_eq!(name.matches('.').count(), 1, "uuid + ext, nothing else: {name}");
        assert!(!name.contains('/') && !name.contains('\\'));
    }

    #[test]
    fn folder_tree_signature_ignores_fields_the_tree_never_draws() {
        // A STATUS pass rewrites uidnext/uidvalidity on every folder every
        // time. If those counted, the guard would fire on every pass and the
        // sidebar would rebuild (collapsing subfolders, dropping the
        // highlight) for changes that are invisible.
        let account = AccountId("acc".into());
        let mut noisy = test_mailbox(&account, "INBOX", 3);
        noisy.uidnext = 999;
        noisy.uidvalidity = UidValidity(42);
        noisy.total = 1234;
        let quiet = folder_tree_signature(&[(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 3)])], &[]);
        let churned = folder_tree_signature(&[(account.clone(), "Me".into(), vec![noisy])], &[]);
        assert_eq!(quiet, churned);
    }

    #[test]
    fn folder_tree_signature_tracks_the_favorites_section() {
        let account = AccountId("acc".into());
        let accounts = [(account.clone(), "Me".into(), vec![test_mailbox(&account, "INBOX", 0)])];
        let without = folder_tree_signature(&accounts, &[]);
        let with = folder_tree_signature(&accounts, &[test_mailbox(&account, "INBOX", 0)]);
        assert_ne!(without, with, "starring a folder adds a whole section and must rebuild");
    }

    #[test]
    fn body_request_matches_the_current_pending_selection() {
        let pending = Some((MailboxId("acc:inbox".into()), Uid(42)));
        assert!(body_request_matches(&MailboxId("acc:inbox".into()), &Uid(42), pending.as_ref()));
        assert!(!body_request_matches(&MailboxId("acc:inbox".into()), &Uid(43), pending.as_ref()));
    }

    #[test]
    fn body_request_is_rejected_when_no_selection_is_pending() {
        assert!(!body_request_matches(&MailboxId("acc:inbox".into()), &Uid(42), None));
    }

    #[test]
    fn mailbox_account_id_splits_the_account_prefix() {
        assert_eq!(
            mailbox_account_id(&MailboxId("/org/gnome/OnlineAccounts/Accounts/account_7:INBOX".into())),
            Some(AccountId("/org/gnome/OnlineAccounts/Accounts/account_7".into()))
        );
        // No account prefix (an id not shaped "account:path").
        assert_eq!(mailbox_account_id(&MailboxId("INBOX".into())), None);
    }

    /// A minimal vCard with one email field, for contact-lookup tests.
    fn test_contact(address: &str) -> VCard {
        VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: Some("uid".into()),
            full_name: Some("Ada Lovelace".into()),
            name: None,
            organization: None,
            title: None,
            emails: vec![lookout_core::EmailField {
                types: vec!["work".into()],
                address: address.to_string(),
            }],
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            note: None,
            birthday: None,
            categories: Vec::new(),
            other: Vec::new(),
        }
    }

    #[test]
    fn find_contact_by_address_matches_across_accounts_case_insensitively() {
        let state = test_state(Vec::new());
        {
            let mut st = state.borrow_mut();
            // The matching card lives in the alphabetically later account,
            // so a match proves the search spans accounts rather than
            // picking a lucky first.
            st.contacts_by_account.insert(
                AccountId("account_b".into()),
                crate::contacts_view::test_snapshot(
                    "Beta",
                    vec![lookout_dav::ContactRecord {
                        href: "/b/ada.vcf".into(),
                        etag: Some("etag-b".into()),
                        card: test_contact("Ada.Lovelace@Example.com"),
                    }],
                ),
            );
            st.contacts_by_account.insert(
                AccountId("account_a".into()),
                crate::contacts_view::test_snapshot(
                    "Alpha",
                    vec![lookout_dav::ContactRecord {
                        href: "/a/grace.vcf".into(),
                        etag: None,
                        card: test_contact("grace@example.com"),
                    }],
                ),
            );
        }
        // Case/whitespace-insensitive exact match on the address string.
        let (account_id, entry) = find_contact_by_address(&state, "  ada.lovelace@example.com ", None).expect("found in an address book");
        assert_eq!(account_id, AccountId("account_b".into()));
        assert_eq!(entry.card.emails[0].address, "Ada.Lovelace@Example.com");
        assert_eq!(entry.href, "/b/ada.vcf");
        assert_eq!(entry.etag.as_deref(), Some("etag-b"));
        // The email's own account is preferred when it also has the contact.
        state
            .borrow_mut()
            .contacts_by_account
            .get_mut(&AccountId("account_a".into()))
            .unwrap()
            .contacts
            .push(lookout_dav::ContactRecord {
                href: "/a/ada.vcf".into(),
                etag: None,
                card: test_contact("ada.lovelace@example.com"),
            });
        let (preferred_id, _) = find_contact_by_address(&state, "ada.lovelace@example.com", Some(&AccountId("account_a".into()))).expect("found");
        assert_eq!(preferred_id, AccountId("account_a".into()));
        // No address book has the address - nothing found.
        assert!(find_contact_by_address(&state, "nobody@example.com", None).is_none());
    }

    #[test]
    fn unified_merge_dedupes_by_mailbox_and_uid_and_sorts_newest_first() {
        let snapshots = HashMap::from([
            (
                MailboxId("a:INBOX".into()),
                vec![summary(Uid(1), "a:INBOX", 2024, 1, 10, 9), summary(Uid(2), "a:INBOX", 2024, 1, 10, 8)],
            ),
            (
                MailboxId("b:INBOX".into()),
                // Duplicate of a:INBOX/2 (same uid, different mailbox) is kept;
                // the doubled a:INBOX/1 in this snapshot must collapse.
                vec![summary(Uid(2), "b:INBOX", 2024, 1, 10, 10), summary(Uid(1), "a:INBOX", 2024, 1, 10, 9)],
            ),
        ]);

        let merged = merge_unified_snapshots(&snapshots);
        let keys: Vec<(String, u32)> = merged.iter().map(|m| (m.mailbox.0.clone(), m.uid.0)).collect();
        // Newest first; a:INBOX/1 appears once despite being in both snapshots.
        assert_eq!(keys, vec![("b:INBOX".into(), 2), ("a:INBOX".into(), 1), ("a:INBOX".into(), 2)]);
    }

    #[test]
    fn unified_merge_breaks_a_date_tie_by_mailbox_then_uid() {
        // Two messages sharing an identical date, in different mailboxes.
        // `HashMap::values()` iteration order is unspecified, so without an
        // explicit tie-break this assertion would be flaky by construction -
        // it must hold regardless of which snapshot the map happens to
        // iterate first.
        let snapshots = HashMap::from([
            (MailboxId("z:INBOX".into()), vec![summary(Uid(1), "z:INBOX", 2024, 1, 10, 9)]),
            (MailboxId("a:INBOX".into()), vec![summary(Uid(5), "a:INBOX", 2024, 1, 10, 9)]),
        ]);
        let keys: Vec<(String, u32)> = merge_unified_snapshots(&snapshots).iter().map(|m| (m.mailbox.0.clone(), m.uid.0)).collect();
        assert_eq!(keys, vec![("a:INBOX".into(), 5), ("z:INBOX".into(), 1)], "equal dates must break on mailbox, not map iteration order");
    }

    /// A minimal `UiState` with one `AccountHandle` per given account (fresh
    /// command channels, empty folder lists unless the caller passes them).
    fn test_state(accounts: Vec<(AccountId, Vec<Mailbox>)>) -> Rc<RefCell<UiState>> {
        let accounts: HashMap<AccountId, AccountHandle> = accounts
            .into_iter()
            .map(|(id, folders)| {
                let (cmd_tx, _cmd_rx) = async_channel::unbounded();
                (
                    id.clone(),
                    AccountHandle {
                        cmd_tx,
                        email: "a@b.c".into(),
                        display_name: String::new(),
                        imap_host: "imap".into(),
                        imap_port: 993,
                        smtp_host: "smtp".into(),
                        smtp_port: 465,
                        folders,
                        address_cache: None,
                    },
                )
            })
            .collect();
        Rc::new(RefCell::new(UiState {
            accounts,
            contacts_by_account: HashMap::new(),
            starred_contacts: HashSet::new(),
            ui_db: None,
            settings: Rc::new(crate::settings::resolve()),
            app_config: Rc::new(RefCell::new(crate::app_config::AppConfig::default())),
            deleted_contacts: HashMap::new(),
            contact_cmd_tx: HashMap::new(),
            goa_accounts: HashMap::new(),
            goa_client: None,
            current_account: None,
            current_mailbox: None,
            mail_view: MailView::Single,
            unified_snapshots: HashMap::new(),
            pending_optimistic_removals: HashMap::new(),
            pending_optimistic_flag_changes: HashMap::new(),
            pending_body_request: None,
            pending_attachment: None,
            pending_raw_message: None,
            unsubscribe_info: None,
            unsubscribe_dismissed: None,
            imip: None,
            imip_dismissed: None,
            read_receipt_request: None,
            read_receipt_dismissed: None,
            read_receipts_sent: HashSet::new(),
            read_receipt_context: None,
            pending_cid: HashMap::new(),
            rendered_inline_parts: Vec::new(),
            temp_attachment_files: HashSet::new(),
            toast_overlay: None,
            sending_toasts: HashMap::new(),
            pending_html_reveal: false,
            pending_header: None,
            body_cache: BodyCache::new(BODY_CACHE_IN_MEMORY),
            reveal_generation: 0,
            last_selection: None,
            restore_pending: false,
            rendered_message: None,
            syncing: HashSet::new(),
            sort_key: SortKey::Date,
            sort_descending: true,
            favorites: HashSet::new(),
            load_remote_images: false,
            rich_text_default: true,
            trusted_senders: HashMap::new(),
            rendered_trust_sender: None,
            load_once_images: false,
            message_theme_override: false,
            trust_banner_dismissed: None,
            rendered_remote_scan: None,
            draft_saved_tx: None,
            composer_identities_refresh: None,
            composer_relay_generation: 0,
            compose_popout_window: None,
            folder_tree: None,
            suppress_folder_selection: false,
            search_active: false,
            search_query: String::new(),
            search_results: Vec::new(),
            search_pending: HashSet::new(),
        }))
    }

    #[test]
    fn request_mailbox_sync_dedupes_until_answered_or_reconnected() {
        let account_id = AccountId("acc".into());
        let inbox = MailboxId("acc:INBOX".into());
        let state = test_state(vec![(account_id.clone(), Vec::new())]);

        // First request goes out and is marked pending.
        assert!(request_mailbox_sync(&state, &account_id, &inbox));
        // A duplicate while the first is still in flight is dropped.
        assert!(!request_mailbox_sync(&state, &account_id, &inbox));

        // The sync landing clears the pending mark, so a later request - a
        // genuine re-sync - is allowed again.
        state.borrow_mut().syncing.remove(&inbox);
        assert!(request_mailbox_sync(&state, &account_id, &inbox));

        // A reconnect (fresh folders) also clears it, so a request that died
        // with a dropped connection can't suppress the next one.
        state.borrow_mut().syncing.insert(inbox.clone());
        state.borrow_mut().syncing.retain(|mailbox| mailbox_account_id(mailbox).as_ref() != Some(&account_id));
        assert!(!state.borrow().syncing.contains(&inbox));
    }

    /// `optimistic_remove_messages` hides a row (and its unified-view
    /// snapshot copy) the instant it's called, stashing it away; a matching
    /// `restore_optimistic_removals` (the `MoveFailed` rollback path) puts it
    /// straight back, in both places, and clears the stash. Same self-skip
    /// convention as the other GTK-touching tests in this module.
    #[test]
    fn optimistic_remove_and_restore_round_trips_the_list_and_unified_snapshot() {
        if gtk::is_initialized() && !gtk::is_initialized_main_thread() {
            return;
        }
        if gtk::init().is_err() {
            return;
        }

        let account_id = AccountId("acc".into());
        let mailbox = MailboxId("acc:INBOX".into());
        let state = test_state(vec![(account_id, Vec::new())]);
        let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
        let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);
        let three = summary(Uid(3), "acc:INBOX", 2026, 8, 1, 11);

        let message_list = MessageListModel::build();
        message_list.repopulate(vec![one.clone(), two.clone(), three.clone()], SortKey::Date, true);
        state.borrow_mut().unified_snapshots.insert(mailbox.clone(), vec![one.clone(), two.clone(), three.clone()]);

        optimistic_remove_messages(&state, &message_list, &mailbox, &[Uid(2)]);
        let uids_after_remove: HashSet<u32> = message_list.all_messages().iter().map(|m| m.uid.0).collect();
        assert_eq!(uids_after_remove, HashSet::from([1, 3]), "the row must vanish immediately, before any server round trip");
        assert_eq!(
            state.borrow().unified_snapshots[&mailbox].iter().map(|m| m.uid.0).collect::<HashSet<_>>(),
            HashSet::from([1, 3]),
            "the unified-view snapshot must lose the row too, or a later merge would resurrect it"
        );
        assert_eq!(
            state.borrow().pending_optimistic_removals[&mailbox].iter().map(|m| m.uid.0).collect::<Vec<_>>(),
            vec![2],
            "the hidden row must be stashed for a possible rollback"
        );

        restore_optimistic_removals(&state, &message_list, &mailbox, &[Uid(2)]);
        let uids_after_restore: HashSet<u32> = message_list.all_messages().iter().map(|m| m.uid.0).collect();
        assert_eq!(uids_after_restore, HashSet::from([1, 2, 3]), "a failed move must restore exactly the row it hid");
        assert_eq!(
            state.borrow().unified_snapshots[&mailbox].iter().map(|m| m.uid.0).collect::<HashSet<_>>(),
            HashSet::from([1, 2, 3]),
            "the unified-view snapshot must get the row back too"
        );
        assert!(
            !state.borrow().pending_optimistic_removals.contains_key(&mailbox),
            "the stash must be empty once its only entry is restored"
        );
    }

    /// `optimistic_toggle_read` flips `SystemFlagBit::Seen` (and the
    /// unified-view snapshot's copy) the instant it's called, stashing the
    /// pre-toggle summary; a matching `restore_optimistic_flag_changes` (the
    /// `StoreFlagsFailed` rollback path) puts the original flags straight
    /// back and clears the stash. Same self-skip convention as the other
    /// GTK-touching tests in this module.
    #[test]
    fn optimistic_toggle_read_and_restore_round_trips_flags_and_unified_snapshot() {
        if gtk::is_initialized() && !gtk::is_initialized_main_thread() {
            return;
        }
        if gtk::init().is_err() {
            return;
        }

        let account_id = AccountId("acc".into());
        let mailbox = MailboxId("acc:INBOX".into());
        let state = test_state(vec![(account_id, Vec::new())]);
        // `summary()` defaults to no flags set, i.e. unread.
        let one = summary(Uid(1), "acc:INBOX", 2026, 8, 1, 9);
        let two = summary(Uid(2), "acc:INBOX", 2026, 8, 1, 10);

        let message_list = MessageListModel::build();
        message_list.repopulate(vec![one.clone(), two.clone()], SortKey::Date, true);
        state.borrow_mut().unified_snapshots.insert(mailbox.clone(), vec![one.clone(), two.clone()]);

        optimistic_toggle_read(&state, &message_list, &mailbox, &[Uid(2)], true);
        let patched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
        assert!(!patched.is_unread(), "the row must flip to read immediately, before any server round trip");
        let untouched = message_list.all_messages().into_iter().find(|m| m.uid == Uid(1)).unwrap();
        assert!(untouched.is_unread(), "an uid not in the toggle set must be untouched");
        assert!(
            !state.borrow().unified_snapshots[&mailbox].iter().find(|m| m.uid == Uid(2)).unwrap().is_unread(),
            "the unified-view snapshot must reflect the toggle too"
        );
        assert_eq!(
            state.borrow().pending_optimistic_flag_changes[&mailbox].iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![Uid(2)],
            "the pre-toggle summary must be stashed for a possible rollback"
        );

        restore_optimistic_flag_changes(&state, &message_list, &mailbox, &[Uid(2)]);
        let restored = message_list.all_messages().into_iter().find(|m| m.uid == Uid(2)).unwrap();
        assert!(restored.is_unread(), "a failed flag update must restore the original unread state");
        assert!(
            state.borrow().unified_snapshots[&mailbox].iter().find(|m| m.uid == Uid(2)).unwrap().is_unread(),
            "the unified-view snapshot must be restored too"
        );
        assert!(
            !state.borrow().pending_optimistic_flag_changes.contains_key(&mailbox),
            "the stash must be empty once its only entry is restored"
        );
    }

    /// The depth-0 row for one account group in a freshly built tree.
    fn account_row(model: &gtk::TreeListModel, id: &AccountId) -> gtk::TreeListRow {
        for i in 0..model.n_items() {
            let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
            if row.depth() != 0 {
                continue;
            }
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
            let tree_item = boxed.borrow::<TreeItem>();
            if let TreeItem::Account(acc) = &*tree_item {
                if &acc.account_id == id {
                    return row;
                }
            }
        }
        panic!("no depth-0 account row for {id:?}");
    }

    /// The depth-0 Favorites section row of a freshly built tree.
    fn favorites_row(model: &gtk::TreeListModel) -> gtk::TreeListRow {
        for i in 0..model.n_items() {
            let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else { continue };
            if row.depth() != 0 {
                continue;
            }
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { continue };
            let tree_item = boxed.borrow::<TreeItem>();
            if matches!(&*tree_item, TreeItem::Favorites) {
                return row;
            }
        }
        panic!("no Favorites row");
    }

    /// The mailbox the folder pane's selection currently highlights, if any.
    fn selected_mailbox(selection: &gtk::SingleSelection) -> Option<MailboxId> {
        let row = selection.selected_item().and_downcast::<gtk::TreeListRow>()?;
        let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let tree_item = boxed.borrow::<TreeItem>();
        match &*tree_item {
            TreeItem::Folder(node) => Some(node.mailbox.id.clone()),
            _ => None,
        }
    }

    /// The folder-pane collapse regression tests, in one `#[test]` for the
    /// same reason `MessageListModel`'s GTK suite is: GTK may only be
    /// initialised on a single thread, and libtest gives each `#[test]` its
    /// own - so several GTK-touching tests race to `gtk::init()` and
    /// whichever loses panics. Skipped when the host has no display to
    /// initialise against. Covers: (1) a user's manual account-group collapse
    /// surviving the constant `FoldersUpdated` rebuilds (with the Favorites
    /// section's own collapse riding along under its reserved key), (2) a
    /// slow-connecting account rendering *expandable but empty* rather than a
    /// chevron-less leaf and popping open when its folders land, and (3) the
    /// end-to-end `rebuild_folder_tree` path preserving collapse, selection,
    /// and scroll.
    #[test]
    fn folder_tree_rebuild_preserves_collapse_state_and_scroll() {
        // GTK's display-dependent tests can only run when the host has a
        // display AND this test's thread is the one that initialized GTK -
        // `gtk::init()` panics if another test thread got there first, so the
        // suite skips instead of failing (same self-skipping convention as
        // `theme.rs`'s `gtk_ok`).
        if gtk::is_initialized() && !gtk::is_initialized_main_thread() {
            return;
        }
        if gtk::init().is_err() {
            return;
        }

        let acc_a = AccountId("acc_a".into());
        let acc_b = AccountId("acc_b".into());

        // --- (1) A user's account-group collapse survives a rebuild ---
        let build = || {
            build_multi_account_tree_model(
                vec![
                    (acc_a.clone(), "A".into(), vec![test_mailbox(&acc_a, "INBOX", 3)]),
                    (acc_b.clone(), "B".into(), vec![test_mailbox(&acc_b, "INBOX", 0)]),
                ],
                vec![test_mailbox(&acc_a, "INBOX", 3)],
            )
        };
        let model = build();
        apply_account_group_expansion(&model, &HashSet::new());
        assert!(account_row(&model, &acc_a).is_expanded(), "default: every account group expanded");
        assert!(account_row(&model, &acc_b).is_expanded());
        assert!(favorites_row(&model).is_expanded());

        // The user collapses account B and the Favorites section.
        account_row(&model, &acc_b).set_expanded(false);
        favorites_row(&model).set_expanded(false);

        let selection = gtk::SingleSelection::new(Some(model));
        let collapsed = collapsed_account_groups(&selection);
        assert!(collapsed.contains(&acc_b));
        assert!(collapsed.contains(&AccountId(FAVORITES_GROUP_KEY.into())));
        assert!(!collapsed.contains(&acc_a), "account A was left expanded");

        // A rebuild (e.g. a count refresh) must put the pane back the way the
        // user left it, not revert everything to all-expanded.
        let rebuilt = build();
        apply_account_group_expansion(&rebuilt, &collapsed);
        assert!(account_row(&rebuilt, &acc_a).is_expanded());
        assert!(!account_row(&rebuilt, &acc_b).is_expanded(), "account B's collapse survived the rebuild");
        assert!(!favorites_row(&rebuilt).is_expanded(), "the Favorites collapse survived too");

        // --- (2) A not-yet-connected account stays expandable-in-waiting ---
        let waiting = build_multi_account_tree_model(
            vec![(acc_a.clone(), "A".into(), vec![test_mailbox(&acc_a, "INBOX", 3)]), (acc_b.clone(), "B".into(), Vec::new())],
            vec![],
        );
        apply_account_group_expansion(&waiting, &HashSet::new());
        let b_row = account_row(&waiting, &acc_b);
        assert!(b_row.is_expandable(), "a not-yet-connected account must not render as a leaf");
        assert!(b_row.is_expanded(), "it defaults to expanded like every account group");

        let selection = gtk::SingleSelection::new(Some(waiting));
        let collapsed = collapsed_account_groups(&selection);
        assert!(!collapsed.contains(&acc_b), "an account still waiting for folders can't be user-collapsed");

        // Its folders arrive on the next sync: the rebuild re-opens it.
        let connected = build_multi_account_tree_model(
            vec![
                (acc_a.clone(), "A".into(), vec![test_mailbox(&acc_a, "INBOX", 4)]),
                (acc_b.clone(), "B".into(), vec![test_mailbox(&acc_b, "INBOX", 1)]),
            ],
            vec![],
        );
        apply_account_group_expansion(&connected, &collapsed);
        let b_row = account_row(&connected, &acc_b);
        assert!(b_row.is_expandable());
        assert!(b_row.is_expanded(), "the account pops open as soon as its folders land");
        assert!(b_row.child_row(0).is_some(), "its INBOX is reachable beneath it");

        // --- (3) End to end through `rebuild_folder_tree` ---
        let state = test_state(vec![
            (acc_a.clone(), vec![test_mailbox(&acc_a, "INBOX", 3)]),
            (acc_b.clone(), vec![test_mailbox(&acc_b, "INBOX", 0)]),
        ]);
        state.borrow_mut().current_account = Some(acc_a.clone());
        state.borrow_mut().current_mailbox = Some(MailboxId("acc_a:INBOX".into()));

        let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
        let folder_scroller = gtk::ScrolledWindow::new();
        folder_scroller.set_vadjustment(Some(&gtk::Adjustment::new(0.0, 0.0, 100.0, 0.0, 0.0, 0.0)));
        let adjustment = folder_scroller.vadjustment();
        adjustment.set_value(42.0);

        rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
        let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
        assert!(account_row(&model, &acc_a).is_expanded());
        assert!(account_row(&model, &acc_b).is_expanded());
        assert_eq!(
            selected_mailbox(&folder_selection),
            Some(MailboxId("acc_a:INBOX".into())),
            "first build restores the open mailbox"
        );

        // The user collapses account B.
        account_row(&model, &acc_b).set_expanded(false);

        // A count refresh lands: the signature changes, so the rebuild runs.
        state.borrow_mut().accounts.get_mut(&acc_b).unwrap().folders[0].unread = 5;
        rebuild_folder_tree(&state, &folder_selection, &folder_scroller);

        let model = folder_selection.model().and_downcast::<gtk::TreeListModel>().expect("tree model");
        assert!(account_row(&model, &acc_a).is_expanded(), "account A stays expanded");
        assert!(!account_row(&model, &acc_b).is_expanded(), "account B's collapse survives the rebuild");
        assert_eq!(selected_mailbox(&folder_selection), Some(MailboxId("acc_a:INBOX".into())), "the selection is restored");
        assert_eq!(adjustment.value(), 42.0, "the scroll position survives the model swap");
    }

    fn summary(uid: Uid, mailbox: &str, year: i32, month: u32, day: u32, hour: u32) -> EmailSummary {
        use chrono::TimeZone;
        use lookout_core::ThreadKey;
        use std::collections::BTreeSet;
        EmailSummary {
            uid,
            mailbox: MailboxId(mailbox.into()),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            thread_key: ThreadKey("t".into()),
            subject: None,
            from: vec![],
            to: vec![],
            cc: vec![],
            date: chrono::Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap(),
            flags: BTreeSet::new(),
            keywords: BTreeSet::new(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            structure: None,
        }
    }

    fn occ(uid: &str, calendar: &str) -> EventOccurrence {
        EventOccurrence {
            uid: lookout_core::EventUid(uid.to_string()),
            calendar_id: CalendarId(calendar.to_string()),
            summary: Some(uid.to_string()),
            description: None,
            location: None,
            start: chrono::Utc::now(),
            end: chrono::Utc::now(),
            all_day: false,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            attendees: vec![],
            organizer: None,
            categories: vec![],
            sensitivity: Default::default(),
            transparency: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    /// The dashboard's per-month occurrence map must never grow beyond the
    /// current and next month - the two months its 14-day horizon needs -
    /// or a long-lived session would accumulate stale month buckets (and a
    /// single-month window is exactly the bug that made the upcoming-events
    /// section drain over time).
    #[test]
    fn dashboard_occurrence_map_prunes_to_the_current_and_next_month() {
        let window = dashboard_month_window();
        assert_eq!(window[0], first_of_month(window[0]), "the window is month-normalized");
        assert_eq!(window[1], window[0] + chrono::Months::new(1), "the window spans current + next month");
        let stale_before = window[0] - chrono::Months::new(1);
        let stale_after = window[1] + chrono::Months::new(1);

        let mut map = HashMap::new();
        insert_dashboard_occurrences(&mut map, stale_before, vec![occ("stale", "cal")]);
        assert!(map.is_empty(), "a stale month is pruned immediately");
        insert_dashboard_occurrences(&mut map, window[0], vec![occ("now", "cal")]);
        insert_dashboard_occurrences(&mut map, window[1], vec![occ("next", "cal")]);
        assert_eq!(map.len(), 2, "only the current and next month survive");
        assert_eq!(map[&window[0]].len(), 1);
        assert_eq!(map[&window[1]].len(), 1);

        // A re-insert of an out-of-window month evicts itself but keeps the
        // in-window months (and their events) intact.
        insert_dashboard_occurrences(&mut map, stale_after, vec![occ("further", "cal")]);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&window[0]));
        assert!(map.contains_key(&window[1]));
    }

    /// A pending drag-move is reapplied onto the incoming occurrence's
    /// start/end for as long as the incoming data doesn't yet reflect it -
    /// covers both the immediate post-drop repaint (still showing the
    /// pre-drag time) and a `sync_month` cache-hit resync that re-broadcasts
    /// stale pre-edit data before the live fetch lands.
    #[test]
    fn pending_calendar_move_is_reapplied_until_the_incoming_data_confirms_it() {
        let t0 = chrono::Utc::now();
        let t1 = t0 + chrono::Duration::hours(1);
        let mut moved = occ("evt-1", "cal");
        moved.start = t1;
        moved.end = t1 + chrono::Duration::hours(1);
        let mut pending = HashMap::from([((moved.uid.clone(), moved.recurrence_id), moved.clone())]);

        // Still-stale incoming data (the original pre-drag time) gets
        // overwritten to the pending value.
        let mut stale = occ("evt-1", "cal");
        stale.start = t0;
        stale.end = t0 + chrono::Duration::hours(1);
        let mut incoming = vec![stale];
        apply_pending_calendar_moves(&mut incoming, &mut pending);
        assert_eq!(incoming[0].start, t1, "the chip must keep showing the dropped time, not the stale cached one");
        assert!(
            pending.contains_key(&(EventUid("evt-1".into()), None)),
            "not yet confirmed by the server - the entry survives"
        );
    }

    #[test]
    fn pending_calendar_move_self_clears_once_the_server_confirms_it() {
        let t1 = chrono::Utc::now();
        let mut moved = occ("evt-1", "cal");
        moved.start = t1;
        moved.end = t1 + chrono::Duration::hours(1);
        let mut pending = HashMap::from([((moved.uid.clone(), moved.recurrence_id), moved.clone())]);

        // The live fetch's own data already matches what we optimistically
        // set - the entry must be dropped rather than kept reapplying.
        let mut incoming = vec![moved.clone()];
        apply_pending_calendar_moves(&mut incoming, &mut pending);
        assert_eq!(incoming[0].start, t1);
        assert!(pending.is_empty(), "a confirmed move must clear the stash");
    }

    #[test]
    fn pending_calendar_move_leaves_unrelated_occurrences_and_empty_pending_untouched() {
        let t0 = chrono::Utc::now();
        let mut other = occ("evt-2", "cal");
        other.start = t0;
        other.end = t0 + chrono::Duration::hours(1);
        let mut incoming = vec![other.clone()];

        // No pending entry for this occurrence's uid at all.
        let mut pending = HashMap::new();
        apply_pending_calendar_moves(&mut incoming, &mut pending);
        assert_eq!(incoming[0].start, t0, "an occurrence with nothing pending must be untouched");

        // A pending entry that belongs to a different occurrence entirely
        // must not affect this one.
        let mut moved = occ("evt-1", "cal");
        moved.start = t0 + chrono::Duration::hours(5);
        moved.end = moved.start + chrono::Duration::hours(1);
        pending.insert((moved.uid.clone(), moved.recurrence_id), moved);
        apply_pending_calendar_moves(&mut incoming, &mut pending);
        assert_eq!(incoming[0].start, t0);
        assert_eq!(pending.len(), 1, "the unrelated pending entry is untouched too - nothing in the incoming list matched it");
    }

    /// A folder row's drop must reach the account session as one
    /// `MoveMessagesTo` per source mailbox, keeping only the dragged
    /// messages that belong to the target folder's own account - the whole
    /// server side of drag-a-message-onto-a-folder, minus the GTK plumbing
    /// (which is exercised manually). Regression guard for the drop path:
    /// the payload arrives as a `G_TYPE_BYTES` value holding the JSON
    /// `(mailbox, uid)` list.
    #[test]
    fn folder_drop_moves_payload_messages_to_the_target_folder() {
        let acc = AccountId("acc".into());
        let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 3), test_mailbox(&acc, "Archive", 0)])]);
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;

        // Two messages from the target's account, one from a different one.
        let payload = serde_json::to_vec(&vec![("acc:INBOX".to_string(), 7u32), ("acc:INBOX".to_string(), 9u32), ("other:INBOX".to_string(), 1u32)]).unwrap();
        let value = glib::Value::from(glib::Bytes::from(payload.as_slice()));
        let target = test_mailbox(&acc, "Archive", 0);

        let handled = handle_message_drag_drop(&state, &target, &value);
        assert!(handled, "a well-formed payload must be claimed by the folder's drop target");
        match cmd_rx.try_recv() {
            Ok(AccountCommand::MoveMessagesTo {
                mailbox,
                uids,
                target: got_target,
            }) => {
                assert_eq!(mailbox, MailboxId("acc:INBOX".into()));
                assert_eq!(uids, vec![Uid(7), Uid(9)], "the other account's message is refused");
                assert_eq!(got_target, MailboxId("acc:Archive".into()));
            }
            other => panic!("expected MoveMessagesTo, got {other:?}"),
        }
        assert!(cmd_rx.try_recv().is_err(), "cross-account messages are dropped, so only one command goes out");
    }

    /// Anything that isn't the message-drag payload must be declined so GTK
    /// can pass the drop on to another target - a foreign app's bytes won't
    /// parse as the JSON shape, and a non-bytes value (e.g. a text drop)
    /// isn't ours at all.
    #[test]
    fn folder_drop_rejects_foreign_payloads() {
        let acc = AccountId("acc".into());
        let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 0)])]);
        let target = test_mailbox(&acc, "Archive", 0);

        assert!(!handle_message_drag_drop(&state, &target, &glib::Value::from("hello")), "a text drop is not our payload");
        assert!(
            !handle_message_drag_drop(&state, &target, &glib::Value::from(glib::Bytes::from_static(b"not json"))),
            "junk bytes are not our payload"
        );
    }

    /// The tag rows' drop side applies the tag to every dragged message of
    /// that account, via one `StoreKeywordsMany` per source mailbox - the
    /// batch counterpart of the Categorize menu's toggles.
    #[test]
    fn tag_drop_stores_the_keyword_on_payload_messages() {
        let acc = AccountId("acc".into());
        let state = test_state(vec![(acc.clone(), vec![test_mailbox(&acc, "INBOX", 3)])]);
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        state.borrow_mut().accounts.get_mut(&acc).unwrap().cmd_tx = cmd_tx;

        let payload = serde_json::to_vec(&vec![("acc:INBOX".to_string(), 7u32)]).unwrap();
        let value = glib::Value::from(glib::Bytes::from(payload.as_slice()));
        assert!(handle_keyword_drag_drop(&state, "work", &value));
        match cmd_rx.try_recv() {
            Ok(AccountCommand::StoreKeywordsMany { mailbox, uids, add, remove }) => {
                assert_eq!(mailbox, MailboxId("acc:INBOX".into()));
                assert_eq!(uids, vec![Uid(7)]);
                assert_eq!(add, vec![lookout_core::tag_keyword("work")]);
                assert!(remove.is_empty());
            }
            other => panic!("expected StoreKeywordsMany, got {other:?}"),
        }
        assert!(cmd_rx.try_recv().is_err(), "only one command goes out");
    }

    /// Coalescing is the point of the bounded event channels: the startup
    /// burst queues the same whole-folder snapshot several times in a row
    /// (cache replay, live sync, previews), and only the last copy of each
    /// snapshot must survive the drain - the UI repaints once per batch, not
    /// once per queued copy.
    #[test]
    fn collapse_account_events_keeps_only_the_last_copy_of_each_snapshot() {
        let a = MailboxId("a:INBOX".into());
        let inbox = |unread: u32| vec![test_mailbox(&AccountId("a".into()), "INBOX", unread)];
        let collapsed = collapse_account_events(vec![
            AccountEvent::FoldersUpdated(inbox(1)),
            AccountEvent::MessagesUpdated {
                mailbox: a.clone(),
                messages: Vec::new(),
            },
            AccountEvent::FoldersUpdated(inbox(2)),
        ]);
        assert_eq!(collapsed.len(), 2, "the earlier folder list is superseded");
        assert!(matches!(&collapsed[0], AccountEvent::MessagesUpdated { mailbox, .. } if mailbox == &a));
        let AccountEvent::FoldersUpdated(folders) = &collapsed[1] else {
            panic!("expected the surviving FoldersUpdated last, got {collapsed:?}");
        };
        assert_eq!(folders[0].unread, 2, "only the newest folder list survives, in its original position");
    }

    #[test]
    fn collapse_account_events_keeps_the_last_messages_updated_per_mailbox_in_order() {
        let a = MailboxId("a:INBOX".into());
        let b = MailboxId("a:Archive".into());
        let msg = |m: MailboxId| AccountEvent::MessagesUpdated { mailbox: m, messages: Vec::new() };
        let collapsed = collapse_account_events(vec![
            AccountEvent::NewMessages {
                mailbox: a.clone(),
                messages: Vec::new(),
            },
            msg(a.clone()),
            msg(b.clone()),
            msg(a.clone()),
        ]);
        assert_eq!(collapsed.len(), 3, "the superseded first copy of A is dropped, the rest stay");
        assert!(matches!(&collapsed[0], AccountEvent::NewMessages { .. }), "non-snapshot events keep their position");
        let mailboxes: Vec<&MailboxId> = collapsed
            .iter()
            .filter_map(|event| match event {
                AccountEvent::MessagesUpdated { mailbox, .. } => Some(mailbox),
                _ => None,
            })
            .collect();
        assert_eq!(mailboxes, vec![&b, &a], "last copy per mailbox, in original order");
    }

    #[test]
    fn collapse_account_events_keeps_everything_when_nothing_is_superseded() {
        let a = MailboxId("a:INBOX".into());
        let collapsed = collapse_account_events(vec![
            AccountEvent::MessagesUpdated {
                mailbox: a.clone(),
                messages: Vec::new(),
            },
            AccountEvent::MoveFailed {
                mailbox: a.clone(),
                uids: vec![Uid(7)],
                role: MailboxRole::Custom,
                message: "nope".into(),
            },
            AccountEvent::StoreFlagsFailed {
                mailbox: a.clone(),
                uids: vec![Uid(7)],
                message: "nope".into(),
            },
        ]);
        assert_eq!(collapsed.len(), 3, "no repeated snapshot key, nothing is dropped");
        assert!(matches!(&collapsed[1], AccountEvent::MoveFailed { .. }));
        assert!(matches!(&collapsed[2], AccountEvent::StoreFlagsFailed { .. }));
    }

    #[test]
    fn collapse_calendar_events_keeps_the_last_occurrences_per_month() {
        let m1 = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let m2 = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let occ = |month: chrono::NaiveDate| CalendarSessionEvent::OccurrencesUpdated { month, occurrences: Vec::new() };
        let collapsed = collapse_calendar_events(vec![CalendarSessionEvent::CalendarsUpdated(Vec::new()), occ(m1), occ(m2), occ(m1)]);
        assert_eq!(collapsed.len(), 3, "the earlier August copy is superseded");
        assert!(matches!(&collapsed[0], CalendarSessionEvent::CalendarsUpdated(_)));
        let months: Vec<&chrono::NaiveDate> = collapsed
            .iter()
            .filter_map(|event| match event {
                CalendarSessionEvent::OccurrencesUpdated { month, .. } => Some(month),
                _ => None,
            })
            .collect();
        assert_eq!(months, vec![&m2, &m1], "one copy per month, in original order");
    }

    #[test]
    fn collapse_calendar_events_keeps_one_calendars_and_tasks_copy() {
        let collapsed = collapse_calendar_events(vec![
            CalendarSessionEvent::CalendarsUpdated(Vec::new()),
            CalendarSessionEvent::TasksUpdated(Vec::new()),
            CalendarSessionEvent::CalendarsUpdated(Vec::new()),
        ]);
        assert_eq!(collapsed.len(), 2);
        assert!(matches!(&collapsed[0], CalendarSessionEvent::TasksUpdated(_)));
        assert!(matches!(&collapsed[1], CalendarSessionEvent::CalendarsUpdated(_)));
    }

    #[test]
    fn collapse_google_tasks_events_keeps_one_list_and_task_snapshot() {
        let collapsed = collapse_google_tasks_events(vec![
            GoogleTasksEvent::ListsUpdated(Vec::new()),
            GoogleTasksEvent::TasksUpdated(Vec::new()),
            GoogleTasksEvent::ListsUpdated(Vec::new()),
        ]);
        assert_eq!(collapsed.len(), 2);
        assert!(matches!(&collapsed[0], GoogleTasksEvent::TasksUpdated(_)));
        assert!(matches!(&collapsed[1], GoogleTasksEvent::ListsUpdated(_)));
    }

    #[test]
    fn collapse_subscription_events_keeps_the_last_update_per_month() {
        let m1 = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let m2 = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let feeds = |month: chrono::NaiveDate| SubscriptionSessionEvent::SubscriptionsUpdated { month, feeds: Vec::new() };
        let collapsed = collapse_subscription_events(vec![feeds(m1), feeds(m2), feeds(m1)]);
        assert_eq!(collapsed.len(), 2, "the earlier August update is superseded");
        let months: Vec<chrono::NaiveDate> = collapsed
            .iter()
            .map(|event| match event {
                SubscriptionSessionEvent::SubscriptionsUpdated { month, .. } => *month,
            })
            .collect();
        assert_eq!(months, vec![m2, m1]);
    }
}
