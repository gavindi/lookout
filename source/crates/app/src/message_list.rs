//! The message list's model layer: what each row *is*, how messages are
//! bucketed into the collapsible date sections the list groups by, and the
//! `Gtk.TreeListModel` bundle that `window.rs` drives.
//!
//! Kept out of `window.rs` because the interesting parts here - bucketing,
//! labelling, layout - are pure functions over `EmailSummary` that can be
//! unit-tested without a GTK main loop, and because the tree model's
//! re-entrancy discipline (see `MessageListModel::repopulate`) is easier to
//! keep honest when it isn't interleaved with 3000 lines of widget wiring.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};
use gtk::prelude::*;
use gtk::{gio, glib};
use lookout_core::{EmailSummary, MailboxId, Uid};

/// Which field the message list is ordered by. Paired with
/// `UiState::sort_descending`, this is the whole of the list's ordering
/// policy - see `sort_messages`. Also decides whether the list is grouped:
/// only `Date` produces section headers (see `build_layout`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Date,
    Sender,
    Subject,
}

impl SortKey {
    /// The sort dropdown's button label for this key.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Date => "By Date",
            SortKey::Sender => "By Sender",
            SortKey::Subject => "By Subject",
        }
    }

    /// The key's name in the `sort-key` `gio::SimpleAction`'s state, which is
    /// what the sort menu's radio items are bound to.
    pub fn action_state(self) -> &'static str {
        match self {
            SortKey::Date => "date",
            SortKey::Sender => "sender",
            SortKey::Subject => "subject",
        }
    }

    pub fn from_action_state(name: &str) -> Option<Self> {
        match name {
            "date" => Some(SortKey::Date),
            "sender" => Some(SortKey::Sender),
            "subject" => Some(SortKey::Subject),
            _ => None,
        }
    }
}

/// Which messages the list shows, applied as a pre-sort subset of the
/// incoming message set. The model keeps the *unfiltered* set as its source
/// of truth (see `MessageListModel::all_messages`), so switching the filter
/// re-renders from the full set rather than from an already-filtered subset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListFilter {
    All,
    Unread,
    Flagged,
}

impl ListFilter {
    /// The filter menu's item label - also the `MenuButton`'s text, mirroring
    /// `SortKey::label`.
    pub fn label(self) -> &'static str {
        match self {
            ListFilter::All => "All",
            ListFilter::Unread => "Unread",
            ListFilter::Flagged => "Flagged",
        }
    }

    /// The filter's name in the `list-filter` `gio::SimpleAction`'s state,
    /// which is what the filter menu's radio items are bound to.
    pub fn action_state(self) -> &'static str {
        match self {
            ListFilter::All => "all",
            ListFilter::Unread => "unread",
            ListFilter::Flagged => "flagged",
        }
    }

    pub fn from_action_state(name: &str) -> Option<Self> {
        match name {
            "all" => Some(ListFilter::All),
            "unread" => Some(ListFilter::Unread),
            "flagged" => Some(ListFilter::Flagged),
            _ => None,
        }
    }

    /// Whether `message` passes this filter. Unread and Flagged are drawn
    /// from the system flags the row itself renders, so the two can overlap -
    /// a flagged-but-unread message belongs to both.
    fn matches(self, message: &EmailSummary) -> bool {
        match self {
            ListFilter::All => true,
            ListFilter::Unread => message.is_unread(),
            ListFilter::Flagged => message.is_starred(),
        }
    }
}

/// The date range one section header covers.
///
/// Deliberately payload-free and `Copy + Eq + Hash`: the set of
/// user-collapsed sections is keyed by this, and a `Month(year, month)`
/// variant would silently orphan that state every time the calendar rolls
/// over into a new month. `LastMonth` carries its name in the *label*
/// instead (see `bucket_label`), so the identity stays stable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DateBucket {
    /// Dated in the future - clock skew or a spammer's forged Date header.
    /// Pinned above `Today` so such messages don't vanish into the middle of
    /// the list.
    Later,
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    /// Labelled with the month's name ("July"), not "Last Month".
    LastMonth,
    Older,
}

/// The Monday on or before `date`.
fn week_start(date: NaiveDate) -> NaiveDate {
    let offset = date.weekday().num_days_from_monday() as i64;
    date - chrono::Duration::days(offset)
}

/// `(year, month)` of the calendar month before `date`'s.
fn previous_month(date: NaiveDate) -> (i32, u32) {
    if date.month() == 1 {
        (date.year() - 1, 12)
    } else {
        (date.year(), date.month() - 1)
    }
}

/// Which section `date` belongs to, relative to `now`.
///
/// Compares *calendar dates in local time* rather than elapsed durations, so
/// a message from 11pm yesterday reads as "Yesterday" rather than "Today"
/// just because it's within 24 hours. The clause order matters: it's what
/// puts the 1st of the current month in `ThisWeek` (where a reader looking
/// for a two-day-old mail expects it) rather than in `ThisMonth`.
pub fn bucket_for(date: DateTime<Utc>, now: DateTime<Local>) -> DateBucket {
    let d = date.with_timezone(&Local).date_naive();
    let today = now.date_naive();

    if d > today {
        return DateBucket::Later;
    }
    if d == today {
        return DateBucket::Today;
    }
    if d == today - chrono::Duration::days(1) {
        return DateBucket::Yesterday;
    }
    let this_week = week_start(today);
    if d >= this_week {
        return DateBucket::ThisWeek;
    }
    if d >= this_week - chrono::Duration::days(7) {
        return DateBucket::LastWeek;
    }
    if (d.year(), d.month()) == (today.year(), today.month()) {
        return DateBucket::ThisMonth;
    }
    if (d.year(), d.month()) == previous_month(today) {
        return DateBucket::LastMonth;
    }
    DateBucket::Older
}

/// The header text for `bucket`. Only `LastMonth` depends on `now` - it
/// renders as the month's own name ("July"), which is why the bucket itself
/// doesn't carry one.
pub fn bucket_label(bucket: DateBucket, now: DateTime<Local>) -> String {
    match bucket {
        DateBucket::Later => "Later".to_string(),
        DateBucket::Today => "Today".to_string(),
        DateBucket::Yesterday => "Yesterday".to_string(),
        DateBucket::ThisWeek => "This Week".to_string(),
        DateBucket::LastWeek => "Last Week".to_string(),
        DateBucket::ThisMonth => "This Month".to_string(),
        DateBucket::LastMonth => {
            let (year, month) = previous_month(now.date_naive());
            NaiveDate::from_ymd_opt(year, month, 1)
                .map(|d| d.format("%B").to_string())
                .unwrap_or_else(|| "Last Month".to_string())
        }
        DateBucket::Older => "Older".to_string(),
    }
}

/// The date column's text for one row: a weekday and time for the last week
/// ("Mon 10:20 PM"), a numeric date beyond that ("15/07/2026").
///
/// Deliberately not locale-formatted (`glib::DateTime`'s `%x`/`%X`) - the
/// two-tier weekday/numeric split is the point, and `%x` would render the
/// recent tier as a bare date with no time. A weekday abbreviation is only
/// unambiguous within a week, which is what sets the cutoff.
pub fn format_row_date(date: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let local = date.with_timezone(&Local);
    if now.signed_duration_since(date) < chrono::Duration::days(7) {
        local.format("%a %-I:%M %p").to_string()
    } else {
        local.format("%-d/%m/%Y").to_string()
    }
}

/// What a `Gtk.TreeListRow`'s `glib::BoxedAnyObject` actually holds. Mirrors
/// `folder_tree::TreeItem`'s approach: a plain Rust enum behind
/// `BoxedAnyObject` rather than a `glib::Object` subclass, because the list
/// is always rebuilt wholesale by `splice` and so never needs per-property
/// change notification.
pub enum MessageItem {
    /// A collapsible date-bucket header.
    Section(SectionRow),
    /// One message. Boxed because `EmailSummary` is large, and an unboxed
    /// variant would size every section row to it too.
    Message(Box<EmailSummary>),
}

pub struct SectionRow {
    pub bucket: DateBucket,
    pub label: String,
}

/// How the list is arranged for a given sort: grouped under date headers, or
/// a flat run of messages.
#[derive(Debug)]
enum ListLayout {
    Flat(Vec<EmailSummary>),
    Grouped(Vec<(DateBucket, String, Vec<EmailSummary>)>),
}

/// Splits an already-sorted message list into the sections the list renders.
///
/// Grouping only makes sense under `SortKey::Date` - sections are date
/// ranges, and under a sender/subject sort their contents would interleave
/// arbitrarily - so the other keys render flat.
///
/// Sections are cut from *consecutive runs* of the sorted slice rather than
/// collected into a map and sorted afterwards. `bucket_for` is monotone in
/// date, so runs come out contiguous and already in the right order - and,
/// for free, an ascending sort yields oldest-section-first, the exact mirror
/// of the descending layout. That's why `DateBucket` needs no `Ord`.
fn build_layout(sorted: Vec<EmailSummary>, key: SortKey, now: DateTime<Local>) -> ListLayout {
    if key != SortKey::Date {
        return ListLayout::Flat(sorted);
    }
    let mut sections: Vec<(DateBucket, String, Vec<EmailSummary>)> = Vec::new();
    for message in sorted {
        let bucket = bucket_for(message.date, now);
        match sections.last_mut() {
            Some((last, _, messages)) if *last == bucket => messages.push(message),
            _ => {
                debug_assert!(
                    !sections.iter().any(|(b, _, _)| *b == bucket),
                    "bucket_for is not monotone in date: {bucket:?} emitted twice"
                );
                sections.push((bucket, bucket_label(bucket, now), vec![message]));
            }
        }
    }
    ListLayout::Grouped(sections)
}

/// Message identity, the two flag-derived styles (unread, flagged), the four
/// text fields a row renders, and the color-tag keywords it carries. Named
/// rather than written inline so the tuple stays under clippy's
/// type-complexity threshold.
type MessageRowKey = (MailboxId, Uid, bool, bool, DateTime<Utc>, Option<String>, String, Option<String>, Vec<String>);

/// A compact fingerprint of the fields one message-list row displays, keyed
/// to keep the row distinct from any other. Used to detect "nothing changed"
/// and skip a rebuild.
///
/// Every field the row renders has to appear here, or a change to it will be
/// mistaken for no change at all: `preview` in particular arrives in a
/// *second* `MessagesUpdated` that is otherwise byte-identical to the first
/// (see `lookout_mail::session::sync_mailbox`'s two-phase sync), so omitting
/// it would silently discard every snippet.
fn message_row_key(m: &EmailSummary) -> MessageRowKey {
    let from = m.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
    // Read *and* flagged state are both part of the key: each drives how the
    // row is drawn, so a `STORE` that only changes a flag must still count as
    // a change - otherwise `repopulate`'s no-op check would swallow the
    // rebuild and the row would keep its old accent bar / flag icon.
    // The `$Lookout-tag-*` keyword subset is here for the same reason: a tag
    // toggle changes only the message's keywords, and the row's tag dots are
    // drawn from them, so without it the rebuild that drops/adds a dot would
    // be skipped. (A tag *recolor/rename* changes no keyword, so it can't be
    // detected here - `MessageListModel::refresh` exists for that.)
    let tags: Vec<String> = m.keywords.iter().filter(|k| lookout_core::tag_key_from_keyword(k).is_some()).cloned().collect();
    (
        m.mailbox.clone(),
        m.uid,
        m.is_unread(),
        m.is_starred(),
        m.date,
        m.subject.clone(),
        from,
        m.preview.clone(),
        tags,
    )
}

/// The list's contents as last rendered, plus the sort and filter that
/// produced them - the comparison `repopulate` skips a rebuild on.
type DisplayedMessages = Option<(SortKey, bool, ListFilter, Vec<EmailSummary>)>;

/// What the message list's selection currently points at. The three cases
/// are genuinely distinct to the reading pane: nothing selected clears it, a
/// section header leaves it alone, and a message drives it - see the
/// selection handler in `window.rs`.
pub enum SelectionKind {
    Empty,
    Section,
    Message(Box<EmailSummary>),
}

/// The message list's model: a two-level `Gtk.TreeListModel` whose root rows
/// are collapsible date sections and whose children are the messages in each,
/// plus the selection over it. Replaces what used to be a flat
/// `gio::ListStore` of messages paired with a `SingleSelection`.
///
/// Cloning is cheap and shares the same underlying model - handlers close
/// over clones freely, exactly as they did over the old store/selection pair.
#[derive(Clone)]
pub struct MessageListModel {
    /// The tree's root level: `Section` rows when grouped, `Message` rows
    /// when flat.
    root: gio::ListStore,
    tree: gtk::TreeListModel,
    pub selection: gtk::SingleSelection,
    /// One child store per bucket, handed out by the tree's create-child
    /// closure. These must outlive any individual rebuild: a `TreeListRow`
    /// that survives a `splice` still holds the child model it was given, so
    /// swapping in a fresh store would leave that row pointing at a discarded
    /// one. Rebuilds splice *into* these instead.
    sections: Rc<RefCell<HashMap<DateBucket, gio::ListStore>>>,
    /// Sections the user has collapsed. Collapse is the exception, so an
    /// absent bucket means expanded and a newly-appearing section defaults
    /// open. Lives outside the model so it survives the constant rebuilds
    /// (cache replay, live sync, on-demand sync) that recreate every row.
    collapsed: Rc<RefCell<HashSet<DateBucket>>>,
    /// What's currently on screen, in display order, and the sort + filter
    /// that produced it. Kept here rather than in `UiState` so `repopulate`
    /// never has to hold a `UiState` borrow across a `splice` - which
    /// synchronously re-enters the selection handler, and would panic on the
    /// re-borrow.
    displayed: Rc<RefCell<DisplayedMessages>>,
    /// The *unfiltered* message set from the last `repopulate` - the source
    /// of truth a filter change re-renders from. `displayed` is derived from
    /// this (filtered, then sorted), so toggling the filter off can bring
    /// back messages the filtered subset no longer contains.
    truth: Rc<RefCell<Vec<EmailSummary>>>,
    /// The active filter, applied inside `repopulate` to every incoming
    /// message set. Owned by the model rather than `UiState` (unlike the
    /// sort), so every caller that rebuilds the list gets the same subset
    /// without threading the filter through each call site.
    filter: Rc<RefCell<ListFilter>>,
}

impl MessageListModel {
    pub fn build() -> Self {
        let root = gio::ListStore::new::<glib::BoxedAnyObject>();
        let sections: Rc<RefCell<HashMap<DateBucket, gio::ListStore>>> = Rc::new(RefCell::new(HashMap::new()));
        let tree = gtk::TreeListModel::new(root.clone(), false, false, {
            let sections = sections.clone();
            move |item| {
                let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
                let item = boxed.borrow::<MessageItem>();
                let MessageItem::Section(section) = &*item else {
                    // A message is a leaf: no child model, no expander.
                    return None;
                };
                let store = sections
                    .borrow_mut()
                    .entry(section.bucket)
                    .or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
                    .clone();
                Some(store.upcast::<gio::ListModel>())
            }
        });
        // `passthrough = false` (as in the folder tree) means the selection
        // and the row factory see `Gtk.TreeListRow`s, which is what drives
        // the section expanders. `autoexpand = false` because expansion is
        // driven from `collapsed`, not left to the model.
        let selection = gtk::SingleSelection::new(Some(tree.clone()));
        MessageListModel {
            root,
            tree,
            selection,
            sections,
            collapsed: Rc::new(RefCell::new(HashSet::new())),
            displayed: Rc::new(RefCell::new(None)),
            truth: Rc::new(RefCell::new(Vec::new())),
            filter: Rc::new(RefCell::new(ListFilter::All)),
        }
    }

    /// Every message the list is currently backed by, before filtering - the
    /// unfiltered source of truth. Lets a sort or filter change re-order /
    /// re-subset the visible list without re-fetching: single-mailbox views
    /// keep no snapshot in `UiState` (only the unified view does), so this is
    /// the only copy of what's being shown.
    pub fn all_messages(&self) -> Vec<EmailSummary> {
        self.truth.borrow().clone()
    }

    /// The selected row's message, or `None` when nothing is selected *or*
    /// the selection has landed on a section header.
    pub fn selected_summary(&self) -> Option<EmailSummary> {
        match self.selection_kind() {
            SelectionKind::Message(summary) => Some(*summary),
            _ => None,
        }
    }

    pub fn selection_kind(&self) -> SelectionKind {
        let Some(row) = self.selection.selected_item().and_downcast::<gtk::TreeListRow>() else {
            return SelectionKind::Empty;
        };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
            return SelectionKind::Empty;
        };
        let item = boxed.borrow::<MessageItem>();
        match &*item {
            MessageItem::Section(_) => SelectionKind::Section,
            MessageItem::Message(summary) => SelectionKind::Message(summary.clone()),
        }
    }

    /// Replaces the list's contents with `messages` in the given sort order,
    /// filtered by the active `ListFilter`, grouped into date sections when
    /// sorting by date, preserving the current highlight and the user's
    /// collapsed sections where possible.
    ///
    /// The *unfiltered* `messages` are kept as the model's source of truth
    /// before the filter is applied, so `set_filter` and the sort controls can
    /// re-derive the display from the full set rather than from an
    /// already-filtered subset.
    ///
    /// A no-op when the incoming list is identical to what's already
    /// displayed: the startup burst - each account's cache replay plus its
    /// live sync plus the app's on-demand syncs - delivers the same envelope
    /// set several times in a row, and rebuilding for each one would
    /// re-select the first row (`GtkSingleSelection` autoselects), refiring
    /// the selection handler and crossfading the same email every time.
    ///
    /// Every model mutation below - each `splice`, each `set_expanded` -
    /// synchronously fires `items-changed`, which reaches the selection
    /// handler and thence `UiState`. So no `RefCell` borrow may be held
    /// across one; each step snapshots what it needs and drops the borrow
    /// first.
    pub fn repopulate(&self, messages: Vec<EmailSummary>, sort_key: SortKey, sort_descending: bool) {
        // Remember the full, unfiltered set first - a filter toggle
        // re-renders from this, so messages hidden by the filter aren't lost.
        *self.truth.borrow_mut() = messages.clone();

        // Apply the active filter, then sort the surviving subset. Filtering
        // first keeps the no-op check comparing like with like: the subset
        // this rebuild produced against the subset already displayed.
        let mut messages = messages;
        let filter = *self.filter.borrow();
        messages.retain(|m| filter.matches(m));
        sort_messages(&mut messages, sort_key, sort_descending);

        // The sort is part of the comparison, not just the contents: a list
        // can be element-identical under ascending and descending order yet
        // still need re-grouping, because the sections come out mirrored. So
        // is the filter - a change to it must rebuild even when the surviving
        // subset happens to be element-identical.
        let unchanged = self.displayed.borrow().as_ref().is_some_and(|(key, descending, shown_filter, current)| {
            *key == sort_key
                && *descending == sort_descending
                && *shown_filter == filter
                && current.len() == messages.len()
                && current.iter().zip(messages.iter()).all(|(a, b)| message_row_key(a) == message_row_key(b))
        });
        if unchanged {
            return;
        }

        // Snapshot what the user has collapsed before anything is spliced.
        self.capture_collapsed();

        // Remember the current highlight before the rebuild wipes it: the
        // splices drop the old row objects, which would otherwise lose the
        // user's selection every time a delete/archive/snooze lands a
        // `MessagesUpdated`.
        let previous_selection = self.selected_summary().map(|s| (s.mailbox.clone(), s.uid));
        let previous_index = self.selection.selected();

        *self.displayed.borrow_mut() = Some((sort_key, sort_descending, filter, messages.clone()));

        match build_layout(messages, sort_key, Local::now()) {
            ListLayout::Flat(messages) => {
                self.clear_sections(&HashSet::new());
                let rows: Vec<glib::Object> = messages
                    .into_iter()
                    .map(|m| glib::BoxedAnyObject::new(MessageItem::Message(Box::new(m))).upcast())
                    .collect();
                self.root.splice(0, self.root.n_items(), &rows);
            }
            ListLayout::Grouped(sections) => {
                // Children before roots: a root row whose child store is
                // already correct spends no time reporting a stale child
                // count to the tree model.
                let live: HashSet<DateBucket> = sections.iter().map(|(b, _, _)| *b).collect();
                for (bucket, _, messages) in &sections {
                    let store = self
                        .sections
                        .borrow_mut()
                        .entry(*bucket)
                        .or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
                        .clone();
                    let rows: Vec<glib::Object> = messages
                        .iter()
                        .map(|m| glib::BoxedAnyObject::new(MessageItem::Message(Box::new(m.clone()))).upcast())
                        .collect();
                    store.splice(0, store.n_items(), &rows);
                }
                self.clear_sections(&live);

                let rows: Vec<glib::Object> = sections
                    .into_iter()
                    .map(|(bucket, label, _)| glib::BoxedAnyObject::new(MessageItem::Section(SectionRow { bucket, label })).upcast())
                    .collect();
                self.root.splice(0, self.root.n_items(), &rows);
                self.apply_expansion();
            }
        }

        self.restore_selection(previous_selection, previous_index);
    }

    /// Switches which messages the list shows and re-renders from the stored
    /// unfiltered set under the current sort, so `Unread` ↔ `All` round-trips
    /// without losing a message. A no-op when the filter is already in effect.
    pub fn set_filter(&self, filter: ListFilter) {
        if *self.filter.borrow() == filter {
            return;
        }
        *self.filter.borrow_mut() = filter;
        let (sort_key, sort_descending) = match *self.displayed.borrow() {
            Some((key, descending, _, _)) => (key, descending),
            // Nothing has ever been rendered; the defaults are what a first
            // repopulate would use anyway.
            None => (SortKey::Date, true),
        };
        let truth = self.all_messages();
        self.repopulate(truth, sort_key, sort_descending);
    }

    /// Re-renders from the unfiltered source of truth under the current sort
    /// and filter, bypassing the "nothing changed" check. Used after tag
    /// definition edits: a rename or recolor changes how existing rows draw
    /// without changing any message's keywords, so `repopulate`'s identity
    /// comparison (`message_row_key`) would see identical input and skip the
    /// rebuild. Clearing `displayed` first makes the no-op check see "nothing
    /// rendered yet" and rebuild unconditionally.
    pub fn refresh(&self) {
        let (sort_key, sort_descending) = match *self.displayed.borrow() {
            Some((key, descending, _, _)) => (key, descending),
            None => (SortKey::Date, true),
        };
        // Clearing `displayed` first makes the no-op check see "nothing
        // rendered yet" and rebuild unconditionally.
        *self.displayed.borrow_mut() = None;
        self.repopulate(self.all_messages(), sort_key, sort_descending);
    }

    /// Empties every section store whose bucket isn't in `live`, so a bucket
    /// that has gone away stops contributing rows. The stores themselves are
    /// kept (see the `sections` field's note).
    fn clear_sections(&self, live: &HashSet<DateBucket>) {
        let stale: Vec<gio::ListStore> = self
            .sections
            .borrow()
            .iter()
            .filter(|(bucket, _)| !live.contains(*bucket))
            .map(|(_, store)| store.clone())
            .collect();
        // Borrow dropped before splicing: `items-changed` re-enters.
        for store in stale {
            store.splice(0, store.n_items(), &[] as &[glib::Object]);
        }
    }

    /// Re-projects the user's collapsed-section set onto the freshly-created
    /// `TreeListRow`s, and re-arms the handler that keeps it up to date. This
    /// is what makes a collapse survive the constant rebuilds.
    ///
    /// Deliberately done here rather than in the row factory's `bind`: bind
    /// only runs for rows in the viewport, so a scrolled-off section would
    /// never contribute its children to the flat model and the list's row
    /// count and scroll extent would be wrong.
    fn apply_expansion(&self) {
        let collapsed = self.collapsed.borrow().clone();
        for i in 0..self.root.n_items() {
            // `child_row` indexes the *root* model, so this walk is immune to
            // the flat row count changing underneath it as each expansion
            // splices children in. Iterating `tree.n_items()` would not be.
            let Some(row) = self.tree.child_row(i) else { continue };
            let Some(bucket) = section_bucket(&row) else { continue };
            row.set_expanded(!collapsed.contains(&bucket));
        }
    }

    /// Records which sections are collapsed right now, so the next rebuild
    /// can put them back. Must run before any splice - the rows it reads are
    /// exactly what the splice destroys.
    ///
    /// Reading the model beats listening to `notify::expanded`, which was the
    /// obvious approach and does not work: `TreeListModel` hands out
    /// `TreeListRow`s on demand and doesn't retain them, so a handler
    /// connected during a rebuild is dropped with its row and never sees the
    /// user's later collapse. The same signal also fires when a splice tears
    /// rows down, which is indistinguishable from a real collapse. The
    /// model's own expansion state has neither problem.
    fn capture_collapsed(&self) {
        let mut collapsed = HashSet::new();
        let mut saw_section = false;
        for i in 0..self.root.n_items() {
            let Some(row) = self.tree.child_row(i) else { continue };
            let Some(bucket) = section_bucket(&row) else { continue };
            saw_section = true;
            if !row.is_expanded() {
                collapsed.insert(bucket);
            }
        }
        // Only a grouped list can speak for the collapsed set. A flat one
        // (a sender/subject sort) has no section rows at all, and letting it
        // write an empty set would forget the user's collapses across a
        // there-and-back sort change.
        if saw_section {
            *self.collapsed.borrow_mut() = collapsed;
        }
    }

    /// Restores the highlight after a rebuild: the same message if it's still
    /// present (the rebuild wasn't a delete of the selected row), otherwise
    /// whatever now occupies its old spot.
    fn restore_selection(&self, previous: Option<(MailboxId, Uid)>, previous_index: u32) {
        let n = self.selection.n_items();
        if n == 0 {
            return;
        }
        if let Some((mailbox, uid)) = previous {
            for i in 0..n {
                if let Some((m, u)) = self.message_at(i) {
                    if m == mailbox && u == uid {
                        self.selection.set_selected(i);
                        return;
                    }
                }
            }
        }
        if previous_index == gtk::INVALID_LIST_POSITION {
            return;
        }
        // Fall back to the old position, walking forward past any section
        // header so the highlight never parks on one.
        let mut index = previous_index.min(n - 1);
        while index < n && self.message_at(index).is_none() {
            index += 1;
        }
        if index < n {
            self.selection.set_selected(index);
        }
    }

    /// The `(mailbox, uid)` of the message at flat position `i`, or `None`
    /// if that row is a section header.
    fn message_at(&self, i: u32) -> Option<(MailboxId, Uid)> {
        let row = self.selection.item(i).and_downcast::<gtk::TreeListRow>()?;
        let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let item = boxed.borrow::<MessageItem>();
        match &*item {
            MessageItem::Message(summary) => Some((summary.mailbox.clone(), summary.uid)),
            MessageItem::Section(_) => None,
        }
    }
}

/// The date bucket a root-level `TreeListRow` heads, or `None` if the row is
/// a message rather than a section.
fn section_bucket(row: &gtk::TreeListRow) -> Option<DateBucket> {
    let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let item = boxed.borrow::<MessageItem>();
    match &*item {
        MessageItem::Section(section) => Some(section.bucket),
        MessageItem::Message(_) => None,
    }
}

/// Orders the message list by the header's chosen key. Every key tie-breaks on
/// date, newest first, so the order is total - two messages with the same
/// sender or subject can't shuffle between otherwise-identical rebuilds and
/// defeat `repopulate`'s identity check.
pub fn sort_messages(messages: &mut [EmailSummary], key: SortKey, descending: bool) {
    fn sender_of(m: &EmailSummary) -> String {
        m.from.first().map(|a| a.display_label().to_lowercase()).unwrap_or_default()
    }
    match key {
        SortKey::Date => messages.sort_by_key(|m| std::cmp::Reverse(m.date)),
        SortKey::Sender => messages.sort_by(|a, b| sender_of(a).cmp(&sender_of(b)).then_with(|| b.date.cmp(&a.date))),
        SortKey::Subject => {
            let subject_of = |m: &EmailSummary| m.subject.clone().unwrap_or_default().to_lowercase();
            messages.sort_by(|a, b| subject_of(a).cmp(&subject_of(b)).then_with(|| b.date.cmp(&a.date)));
        }
    }
    // Ascending is the descending order reversed rather than a second set of
    // comparators - which also flips the date tie-break, so "oldest first"
    // really is the exact mirror of what was on screen.
    if !descending {
        messages.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn utc_on(now: DateTime<Local>, y: i32, m: u32, d: u32) -> DateTime<Utc> {
        // Noon local, so the value can't slide across a date boundary when
        // converted back for comparison, whatever the test machine's zone.
        let _ = now;
        Local.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap().with_timezone(&Utc)
    }

    /// The reference screenshot: captured 4 Aug 2026, a message from 15 July
    /// sits under "July" while one from 22 February - the same year - sits
    /// under "Older". The named-month bucket is last month only.
    #[test]
    fn last_month_is_named_and_earlier_months_are_older() {
        let now = local(2026, 8, 4, 8);
        assert_eq!(bucket_for(utc_on(now, 2026, 7, 15), now), DateBucket::LastMonth);
        assert_eq!(bucket_label(DateBucket::LastMonth, now), "July");
        assert_eq!(bucket_for(utc_on(now, 2026, 2, 22), now), DateBucket::Older);
        assert_eq!(bucket_for(utc_on(now, 2025, 12, 4), now), DateBucket::Older);
    }

    /// Drives the real `TreeListModel` end to end: default expansion, a
    /// collapse surviving rebuilds, and the flat (non-date) layout.
    ///
    /// One test rather than three because GTK may only be initialised on a
    /// single thread, and libtest gives each `#[test]` its own - so several
    /// GTK-touching tests race to `gtk::init()` and whichever loses panics.
    /// Skipped when the host has no display to initialise against.
    #[test]
    fn tree_model_expansion_and_layout() {
        if gtk::init().is_err() {
            return;
        }
        let today = Utc::now();
        let yesterday = today - chrono::Duration::days(1);
        let older = today - chrono::Duration::days(400);

        // --- Sections are expanded by default ---
        let model = MessageListModel::build();
        model.repopulate(vec![summary(1, today), summary(2, older)], SortKey::Date, true);
        // Two headers + two messages, all in the flat model the ListView
        // renders; a collapsed section would contribute its header alone.
        assert_eq!(model.selection.n_items(), 4, "sections did not expand");
        let kinds: Vec<bool> = (0..model.selection.n_items()).map(|i| model.message_at(i).is_some()).collect();
        assert_eq!(kinds, vec![false, true, false, true], "expected header/message/header/message");

        // A section appearing in a *later* rebuild also defaults open. This
        // is the case the `rebuilding` guard exists for: without it, the
        // root splice's teardown of the rows it replaces fired
        // `notify::expanded` and recorded a collapse nobody asked for.
        model.repopulate(vec![summary(1, today), summary(3, yesterday), summary(2, older)], SortKey::Date, true);
        assert_eq!(model.selection.n_items(), 6, "a newly-appearing section did not expand");
        assert!(model.collapsed.borrow().is_empty(), "a rebuild recorded a phantom collapse");

        // --- A user's collapse outlives the constant rebuilds ---
        model.tree.child_row(0).expect("root row 0").set_expanded(false);
        assert_eq!(model.selection.n_items(), 5, "collapsing hid nothing");
        // A rebuild with different contents, so the identity check can't
        // short-circuit it.
        model.repopulate(vec![summary(1, today), summary(4, today), summary(3, yesterday), summary(2, older)], SortKey::Date, true);
        assert!(!model.tree.child_row(0).expect("root row 0").is_expanded(), "the collapse did not survive a rebuild");
        // 3 headers + Yesterday's 1 + Older's 1; Today's 2 stay hidden.
        assert_eq!(model.selection.n_items(), 5, "only the collapsed section should be hidden");
        model.tree.child_row(0).expect("root row 0").set_expanded(true);
        assert_eq!(model.selection.n_items(), 7, "re-expanding did not restore the messages");

        // --- A non-date sort renders flat, with no headers at all ---
        let flat = MessageListModel::build();
        flat.repopulate(vec![summary(1, today), summary(2, older)], SortKey::Sender, true);
        assert_eq!(flat.selection.n_items(), 2);
        assert!((0..2).all(|i| flat.message_at(i).is_some()), "a sender sort produced a header row");
        // Switching back to date regroups, expanded.
        flat.repopulate(vec![summary(1, today), summary(2, older)], SortKey::Date, true);
        assert_eq!(flat.selection.n_items(), 4);

        // --- A filter renders a subset and re-renders from the unfiltered
        // truth when it changes ---
        let filtered = MessageListModel::build();
        let seen = {
            let mut s = summary(1, today);
            s.flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Seen]);
            s
        };
        let unread = summary(2, older);
        let flagged_unread = {
            let mut s = summary(3, today);
            s.flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Flagged]);
            s
        };
        let flagged_read = {
            let mut s = summary(4, today);
            s.flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Seen, lookout_core::SystemFlagBit::Flagged]);
            s
        };
        let full = vec![seen.clone(), unread.clone(), flagged_unread.clone(), flagged_read.clone()];
        filtered.repopulate(full.clone(), SortKey::Date, true);
        assert_eq!(filtered.selection.n_items(), 6, "unfiltered: two sections + four messages");
        assert_eq!(filtered.all_messages().len(), 4, "the unfiltered truth is kept");

        filtered.set_filter(ListFilter::Unread);
        // `seen` and `flagged_read` are read; the two unread survive.
        assert_eq!(filtered.selection.n_items(), 4, "Unread: two sections + two messages");
        assert_eq!(filtered.all_messages().len(), 4, "filtering never discards the truth");

        filtered.set_filter(ListFilter::Flagged);
        // Both flagged messages, read or not, in the single Today section.
        assert_eq!(filtered.selection.n_items(), 3, "Flagged: one section + two messages");

        // A repopulate carrying the same full set under the same filter is
        // the identity case - the rebuild is skipped, so the row count (and
        // the selection that goes with it) is untouched.
        filtered.repopulate(full.clone(), SortKey::Date, true);
        assert_eq!(filtered.selection.n_items(), 3);

        // Switching back to All restores everything from the truth.
        filtered.set_filter(ListFilter::All);
        assert_eq!(filtered.selection.n_items(), 6, "switching back to All restored the hidden rows");
    }

    #[test]
    fn recent_buckets() {
        // Tuesday 4 Aug 2026.
        let now = local(2026, 8, 4, 8);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 5), now), DateBucket::Later);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 4), now), DateBucket::Today);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 3), now), DateBucket::Yesterday);
    }

    #[test]
    fn week_boundaries_take_precedence_over_the_month() {
        // Tuesday 4 Aug 2026; this week starts Monday 3 Aug, so 1-2 Aug fall
        // in *last* week even though they're in the current month.
        let now = local(2026, 8, 4, 8);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 2), now), DateBucket::LastWeek);
        assert_eq!(bucket_for(utc_on(now, 2026, 7, 28), now), DateBucket::LastWeek);
        // Two weeks back is out of both week buckets, and out of the month.
        assert_eq!(bucket_for(utc_on(now, 2026, 7, 20), now), DateBucket::LastMonth);

        // Mid-month, so "this week" sits wholly inside the current month.
        let now = local(2026, 8, 20, 8);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 17), now), DateBucket::ThisWeek);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 10), now), DateBucket::LastWeek);
        assert_eq!(bucket_for(utc_on(now, 2026, 8, 3), now), DateBucket::ThisMonth);
    }

    #[test]
    fn month_bucket_rolls_over_the_year() {
        // 15 Jan 2027: last month is December *2026*.
        let now = local(2027, 1, 15, 8);
        assert_eq!(bucket_for(utc_on(now, 2026, 12, 10), now), DateBucket::LastMonth);
        assert_eq!(bucket_label(DateBucket::LastMonth, now), "December");
        assert_eq!(bucket_for(utc_on(now, 2026, 11, 10), now), DateBucket::Older);
    }

    fn summary(uid: u32, date: DateTime<Utc>) -> EmailSummary {
        EmailSummary {
            uid: Uid(uid),
            mailbox: MailboxId("acct:INBOX".into()),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: None,
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            date,
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            preview: None,
        }
    }

    #[test]
    fn non_date_sorts_render_flat() {
        let now = local(2026, 8, 4, 8);
        let messages = vec![summary(1, utc_on(now, 2026, 8, 4)), summary(2, utc_on(now, 2026, 2, 1))];
        assert!(matches!(build_layout(messages.clone(), SortKey::Sender, now), ListLayout::Flat(m) if m.len() == 2));
        assert!(matches!(build_layout(messages, SortKey::Subject, now), ListLayout::Flat(m) if m.len() == 2));
    }

    #[test]
    fn date_sort_cuts_one_section_per_run() {
        let now = local(2026, 8, 4, 8);
        // Newest-first, as `sort_messages` leaves a descending Date sort.
        let messages = vec![
            summary(1, utc_on(now, 2026, 8, 4)),
            summary(2, utc_on(now, 2026, 8, 4)),
            summary(3, utc_on(now, 2026, 7, 15)),
            summary(4, utc_on(now, 2026, 2, 22)),
        ];
        let ListLayout::Grouped(sections) = build_layout(messages, SortKey::Date, now) else {
            panic!("expected a grouped layout");
        };
        let shape: Vec<(DateBucket, &str, usize)> = sections.iter().map(|(b, l, m)| (*b, l.as_str(), m.len())).collect();
        assert_eq!(
            shape,
            vec![(DateBucket::Today, "Today", 2), (DateBucket::LastMonth, "July", 1), (DateBucket::Older, "Older", 1)]
        );
    }

    /// An ascending sort is the descending layout mirrored, sections and all -
    /// which falls out of cutting runs rather than sorting buckets.
    #[test]
    fn ascending_dates_yield_oldest_section_first() {
        let now = local(2026, 8, 4, 8);
        let messages = vec![
            summary(4, utc_on(now, 2026, 2, 22)),
            summary(3, utc_on(now, 2026, 7, 15)),
            summary(1, utc_on(now, 2026, 8, 4)),
        ];
        let ListLayout::Grouped(sections) = build_layout(messages, SortKey::Date, now) else {
            panic!("expected a grouped layout");
        };
        let buckets: Vec<DateBucket> = sections.iter().map(|(b, _, _)| *b).collect();
        assert_eq!(buckets, vec![DateBucket::Older, DateBucket::LastMonth, DateBucket::Today]);
    }

    /// True when two ordered message lists are display-identical - the pure
    /// core of `repopulate`'s rebuild-skipping check, testable without a
    /// widget tree.
    fn same_message_list(a: &[EmailSummary], b: &[EmailSummary]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| message_row_key(x) == message_row_key(y))
    }

    fn at(uid: u32, mailbox: &str, y: i32, m: u32, d: u32, h: u32) -> EmailSummary {
        let mut s = summary(uid, Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap());
        s.mailbox = MailboxId(mailbox.into());
        s
    }

    #[test]
    fn identical_message_lists_are_skipped_but_any_displayed_change_is_not() {
        let a = vec![at(2, "a:INBOX", 2024, 1, 10, 10), at(1, "a:INBOX", 2024, 1, 10, 9)];
        // The startup burst: the same envelope set replayed by cache + live
        // sync + on-demand sync. Display-identical, so no repopulate needed.
        assert!(same_message_list(&a, &a));

        // A message arriving (extra row) is a real change.
        let with_new = vec![at(3, "a:INBOX", 2024, 1, 10, 11), at(2, "a:INBOX", 2024, 1, 10, 10), at(1, "a:INBOX", 2024, 1, 10, 9)];
        assert!(!same_message_list(&a, &with_new));

        // A re-order of the same messages is a real change.
        let reordered = vec![at(1, "a:INBOX", 2024, 1, 10, 9), at(2, "a:INBOX", 2024, 1, 10, 10)];
        assert!(!same_message_list(&a, &reordered));

        // An unread->read flip is a real change (the row's emphasis changes).
        let mut read_flip = a.clone();
        read_flip[0].flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Seen]);
        assert!(!same_message_list(&a, &read_flip));

        // A preview arriving is a real change - the two-phase sync's second
        // event differs from the first in nothing else, so if this compared
        // equal no snippet would ever reach the screen.
        let mut with_preview = a.clone();
        with_preview[0].preview = Some("Truffle Security Co. says it scanned...".into());
        assert!(!same_message_list(&a, &with_preview));
    }

    fn uids(messages: &[EmailSummary]) -> Vec<u32> {
        messages.iter().map(|m| m.uid.0).collect()
    }

    fn with_sender_and_subject(mut summary: EmailSummary, sender: &str, subject: &str) -> EmailSummary {
        summary.from = vec![lookout_core::EmailAddress {
            name: Some(sender.to_string()),
            address: format!("{}@example.com", sender.to_lowercase()),
        }];
        summary.subject = Some(subject.to_string());
        summary
    }

    #[test]
    fn sort_messages_orders_by_the_chosen_key_and_mirrors_exactly_when_ascending() {
        // Same date on the two "b"/"c" rows so the sender/subject keys are what
        // actually decides their order, not the date tie-break.
        let mut messages = vec![
            with_sender_and_subject(at(1, "a:INBOX", 2024, 1, 10, 9), "Carol", "Zebra"),
            with_sender_and_subject(at(2, "a:INBOX", 2024, 1, 10, 11), "alice", "middle"),
            with_sender_and_subject(at(3, "a:INBOX", 2024, 1, 10, 10), "Bob", "Apple"),
        ];

        sort_messages(&mut messages, SortKey::Date, true);
        assert_eq!(uids(&messages), vec![2, 3, 1]);

        // Sender and subject are case-insensitive - "alice" must lead "Bob".
        sort_messages(&mut messages, SortKey::Sender, true);
        assert_eq!(uids(&messages), vec![2, 3, 1]);
        sort_messages(&mut messages, SortKey::Subject, true);
        assert_eq!(uids(&messages), vec![3, 2, 1]);

        // Ascending is the exact mirror of descending, tie-break included.
        sort_messages(&mut messages, SortKey::Sender, false);
        assert_eq!(uids(&messages), vec![1, 3, 2]);
        sort_messages(&mut messages, SortKey::Date, false);
        assert_eq!(uids(&messages), vec![1, 3, 2]);
    }

    #[test]
    fn sort_messages_tie_breaks_equal_keys_by_date_so_rebuilds_are_stable() {
        // Two messages the sender key can't tell apart: only the date decides,
        // and it must decide the same way every time - otherwise identical
        // rebuilds could shuffle and defeat the rebuild-skipping check.
        let older = with_sender_and_subject(at(1, "a:INBOX", 2024, 1, 10, 9), "Dana", "one");
        let newer = with_sender_and_subject(at(2, "a:INBOX", 2024, 1, 10, 12), "Dana", "two");

        let mut forwards = vec![older.clone(), newer.clone()];
        let mut backwards = vec![newer, older];
        sort_messages(&mut forwards, SortKey::Sender, true);
        sort_messages(&mut backwards, SortKey::Sender, true);
        assert_eq!(uids(&forwards), vec![2, 1]);
        assert_eq!(uids(&forwards), uids(&backwards));
    }

    #[test]
    fn row_date_switches_from_weekday_to_numeric_after_a_week() {
        let now = Local.with_ymd_and_hms(2026, 8, 4, 8, 0, 0).unwrap();
        let recent = Local.with_ymd_and_hms(2026, 8, 3, 22, 20, 0).unwrap().with_timezone(&Utc);
        assert_eq!(format_row_date(recent, now.with_timezone(&Utc)), "Mon 10:20 PM");
        let old = Local.with_ymd_and_hms(2026, 7, 15, 9, 0, 0).unwrap().with_timezone(&Utc);
        assert_eq!(format_row_date(old, now.with_timezone(&Utc)), "15/07/2026");
        // No leading zero on the day, per the reference screenshot.
        let older = Local.with_ymd_and_hms(2025, 12, 4, 9, 0, 0).unwrap().with_timezone(&Utc);
        assert_eq!(format_row_date(older, now.with_timezone(&Utc)), "4/12/2025");
    }

    #[test]
    fn list_filter_matches_read_and_flagged_state() {
        let mut read = summary(1, Utc::now());
        read.flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Seen]);
        assert!(ListFilter::All.matches(&read));
        assert!(!ListFilter::Unread.matches(&read));
        assert!(!ListFilter::Flagged.matches(&read));

        let unread = summary(2, Utc::now());
        assert!(ListFilter::Unread.matches(&unread));
        assert!(!ListFilter::Flagged.matches(&unread));

        let mut flagged = summary(3, Utc::now());
        flagged.flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Flagged]);
        assert!(ListFilter::Unread.matches(&flagged), "a flagged-but-unread message belongs to both filters");
        assert!(ListFilter::Flagged.matches(&flagged));

        let mut flagged_read = summary(4, Utc::now());
        flagged_read.flags = std::collections::BTreeSet::from([lookout_core::SystemFlagBit::Seen, lookout_core::SystemFlagBit::Flagged]);
        assert!(ListFilter::Flagged.matches(&flagged_read));
        assert!(!ListFilter::Unread.matches(&flagged_read));

        // The menu wiring's label/state round-trips.
        assert_eq!(ListFilter::All.label(), "All");
        assert_eq!(ListFilter::from_action_state(ListFilter::Flagged.action_state()), Some(ListFilter::Flagged));
        assert_eq!(ListFilter::from_action_state("bogus"), None);
    }
}
