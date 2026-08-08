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
use lookout_core::{EmailSummary, MailboxId, ThreadKey, Uid};

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
/// instead (see `bucket_label`), so the identity stays stable. `Year` is the
/// one exception to being payload-free - an old message's calendar year is
/// fixed for good, so a collapse recorded against `Year(2023)` can never
/// silently stop matching.
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
    /// From a bit over a month to a year ago - not yet old enough for its own
    /// year header.
    Older,
    /// Dated more than a year ago, labelled with the message's own calendar
    /// year ("2024"). `LastMonth` and `Older` shade into this as time passes,
    /// but a message already in one `Year` never moves to another.
    Year(i32),
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
    // Older than a year: give the message its own year header ("2024")
    // rather than lumping every old mail under one "Older" section. The
    // cutoff is by age (a December 2025 mail is still "Older" in August
    // 2026), not by calendar year.
    if let Some(cutoff) = today.checked_sub_months(chrono::Months::new(12)) {
        if d < cutoff {
            return DateBucket::Year(d.year());
        }
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
        DateBucket::Year(year) => format!("{year}"),
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
    /// A collapsible conversation header - the date section's children when
    /// threading is on, itself the parent of its messages' rows.
    Thread(ThreadRow),
    /// One message. Boxed because `EmailSummary` is large, and an unboxed
    /// variant would size every section row to it too.
    Message(Box<EmailSummary>),
}

pub struct SectionRow {
    pub bucket: DateBucket,
    pub label: String,
}

/// Thread identity: the `(mailbox, thread key)` pair the messages of one
/// conversation share. Mailbox-prefixed so identical `ThreadKey`s from
/// different mailboxes - the `subject:`-normalized fallback keys above all -
/// can't merge unrelated messages in the unified "All Inboxes" view, where
/// every mailbox's key set was computed independently by
/// `lookout_core::thread::compute_thread_keys`.
pub type ThreadId = (MailboxId, ThreadKey);

/// The header row of one collapsible conversation, derived from the thread's
/// newest message (the one its section placement and the row's date column
/// follow) plus the aggregate state of its whole message set.
#[derive(Clone, Debug)]
pub struct ThreadRow {
    pub id: ThreadId,
    /// The newest message's sender, rendered like a message row's sender.
    pub sender: String,
    /// Every participant's display name, newest-message first - the header's
    /// tooltip, so who's in the conversation is a hover away.
    pub participants: Vec<String>,
    /// The newest message's subject. `None` (→ "(no subject)") matches how a
    /// plain message row renders.
    pub subject: Option<String>,
    /// How many messages the conversation contains. Hidden for the
    /// count-1 case - a lone message renders as itself, not as a thread.
    pub count: usize,
    /// The newest message's date - the header's date column.
    pub latest: DateTime<Utc>,
    /// Any message in the thread is unread - the header bolds like an unread
    /// message row does.
    pub has_unread: bool,
    /// Any message in the thread is flagged - the header shows the flag icon.
    pub has_starred: bool,
    /// Any message in the thread has an attachment - the header shows the
    /// attachment icon, mirroring how a lone message row renders its own.
    pub has_attachment: bool,
    /// Any message in the thread carries an iCalendar part (a meeting invite
    /// or related message) - the header shows the calendar icon.
    pub has_calendar: bool,
}

/// One row a date section's child store holds in threaded mode: a
/// multi-message conversation (a collapsible `Thread` header) or a lone
/// message (rendered exactly as in unthreaded mode, since a thread of one
/// would be a header that exists only to hide its single child).
#[derive(Debug)]
enum ThreadEntry {
    Thread(ThreadLayout),
    Single(EmailSummary),
}

/// A conversation in the display layout: the header row that goes in the
/// section's store, plus the messages that fill its child store.
#[derive(Debug)]
struct ThreadLayout {
    row: ThreadRow,
    /// The thread's messages, oldest-first (natural reading order for a
    /// conversation). Kept here rather than in `ThreadRow` because the boxed
    /// row object never needs the messages themselves - only the splice that
    /// builds the child store does.
    messages: Vec<EmailSummary>,
}

/// How the list is arranged for a given sort: grouped under date headers, a
/// flat run of messages, or date headers over collapsible conversations.
#[derive(Debug)]
enum ListLayout {
    Flat(Vec<EmailSummary>),
    Grouped(Vec<(DateBucket, String, Vec<EmailSummary>)>),
    Threaded(Vec<(DateBucket, String, Vec<ThreadEntry>)>),
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
///
/// With `threaded` set, conversations (`group_threads`) are cut instead of
/// bare messages, and each conversation sits in the section of its *newest*
/// message's date - a two-day-old root whose reply landed today belongs to
/// Today, not to the root's bucket. The section run-cut stays valid because
/// each thread's newest message is the first of its members in the
/// newest-first scan, so the per-thread latest dates the cuts follow are
/// monotone in the same way single messages' dates were.
fn build_layout(sorted: Vec<EmailSummary>, key: SortKey, now: DateTime<Local>, threaded: bool, descending: bool) -> ListLayout {
    if key != SortKey::Date {
        return ListLayout::Flat(sorted);
    }
    let sections: Vec<(DateBucket, String, Vec<ThreadEntry>)> = if threaded {
        // `group_threads` scans newest-first (it needs each thread's newest
        // member to place it), so an ascending sort is mirrored by reversing
        // the scan order first and then the finished sections, exactly as the
        // run-cut below mirrors itself.
        let mut messages = sorted;
        if !descending {
            messages.reverse();
        }
        let mut sections: Vec<(DateBucket, String, Vec<ThreadEntry>)> = Vec::new();
        for entry in group_threads(&messages) {
            let latest = match &entry {
                ThreadEntry::Thread(thread) => thread.row.latest,
                ThreadEntry::Single(message) => message.date,
            };
            let bucket = bucket_for(latest, now);
            match sections.last_mut() {
                Some((last, _, entries)) if *last == bucket => entries.push(entry),
                _ => sections.push((bucket, bucket_label(bucket, now), vec![entry])),
            }
        }
        if !descending {
            // Mirror the descending layout: sections oldest-first, threads
            // within a section by oldest latest first. A thread's own message
            // order is *not* reversed - oldest-first is the natural reading
            // order of a conversation whichever way the list sorts.
            sections.reverse();
            for (_, _, entries) in &mut sections {
                entries.reverse();
            }
        }
        sections
    } else {
        let mut sections: Vec<(DateBucket, String, Vec<ThreadEntry>)> = Vec::new();
        for message in sorted {
            let bucket = bucket_for(message.date, now);
            match sections.last_mut() {
                Some((last, _, entries)) if *last == bucket => entries.push(ThreadEntry::Single(message)),
                _ => {
                    debug_assert!(
                        !sections.iter().any(|(b, _, _)| *b == bucket),
                        "bucket_for is not monotone in date: {bucket:?} emitted twice"
                    );
                    sections.push((bucket, bucket_label(bucket, now), vec![ThreadEntry::Single(message)]));
                }
            }
        }
        sections
    };
    if !threaded {
        // The unthreaded layout keeps its old shape: sections of bare
        // messages.
        let grouped = sections
            .into_iter()
            .map(|(bucket, label, entries)| {
                let messages = entries.into_iter().map(|e| match e {
                    ThreadEntry::Single(message) => message,
                    ThreadEntry::Thread(_) => unreachable!("unthreaded layout never holds thread entries"),
                });
                (bucket, label, messages.collect())
            })
            .collect();
        return ListLayout::Grouped(grouped);
    }
    ListLayout::Threaded(sections)
}

/// Splits a newest-first message list into display entries, in scan order:
/// multi-message conversations as `Thread` headers (their messages carried
/// oldest-first) and lone messages as `Single`s.
///
/// A conversation's identity is the `(mailbox, thread key)` pair the
/// summaries already carry (`EmailSummary::thread_key`, computed by
/// `lookout_core::thread::compute_thread_keys` at sync time and persisted in
/// the envelope cache), so no re-derivation is needed here - and the key's
/// stability across re-syncs is what lets a thread's collapse state (keyed by
/// the same pair) survive the constant rebuilds. A thread of one renders as
/// its message, not a header: a header whose only child is itself adds a row
/// to every conversation without earning its keep.
fn group_threads(messages: &[EmailSummary]) -> Vec<ThreadEntry> {
    let mut threads: HashMap<ThreadId, Vec<EmailSummary>> = HashMap::new();
    let mut order: Vec<ThreadId> = Vec::new();
    for message in messages {
        let id = (message.mailbox.clone(), message.thread_key.clone());
        if !threads.contains_key(&id) {
            order.push(id.clone());
        }
        threads.entry(id).or_default().push(message.clone());
    }
    let mut entries = Vec::with_capacity(order.len());
    for id in order {
        // Newest-first (inserted in scan order), so the first member is the
        // one the header and the section placement follow.
        let mut emails = threads.remove(&id).expect("order only lists inserted ids");
        // An empty key means no sync ever computed one (a summary read from
        // an old cache, or one that never went through `sync_mailbox`).
        // Grouping it would merge every such message into one conversation,
        // so each renders as a lone row instead.
        if id.1 .0.is_empty() {
            entries.extend(emails.into_iter().map(ThreadEntry::Single));
            continue;
        }
        let latest = emails.remove(0);
        if emails.is_empty() {
            entries.push(ThreadEntry::Single(latest));
            continue;
        }
        // The child store carries the *whole* conversation, oldest-first (the
        // natural reading order); `latest` goes back on as the newest member.
        emails.reverse();
        emails.push(latest.clone());
        let sender = latest.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
        let mut participants: Vec<String> = Vec::new();
        for message in &emails {
            for address in &message.from {
                let label = address.display_label().to_string();
                if !participants.contains(&label) {
                    participants.push(label);
                }
            }
        }
        let row = ThreadRow {
            id,
            sender,
            participants,
            subject: latest.subject.clone(),
            count: emails.len(),
            latest: latest.date,
            has_unread: latest.is_unread() || emails.iter().any(|m| m.is_unread()),
            has_starred: latest.is_starred() || emails.iter().any(|m| m.is_starred()),
            has_attachment: latest.has_attachment || emails.iter().any(|m| m.has_attachment),
            has_calendar: latest.has_calendar || emails.iter().any(|m| m.has_calendar),
        };
        entries.push(ThreadEntry::Thread(ThreadLayout { row, messages: emails }));
    }
    entries
}

/// Message identity, the two flag-derived styles (unread, flagged), the four
/// text fields a row renders, the color-tag keywords it carries, and its
/// thread key. Named rather than written inline so the tuple stays under
/// clippy's type-complexity threshold.
type MessageRowKey = (MailboxId, Uid, bool, bool, DateTime<Utc>, Option<String>, String, Option<String>, Vec<String>, ThreadKey);

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
    // `thread_key` decides which conversation a row belongs to when the list
    // is threaded, so a re-sync that re-groups messages must count as a
    // change even when every rendered field is identical.
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
        m.thread_key.clone(),
    )
}

/// The list's contents as last rendered, plus the sort, filter, and threading
/// mode that produced them - the comparison `repopulate` skips a rebuild on.
type DisplayedMessages = Option<(SortKey, bool, ListFilter, bool, Vec<EmailSummary>)>;

/// What the message list's selection currently points at. The cases are
/// genuinely distinct to the reading pane: nothing selected clears it, a
/// section header leaves it alone (unreachable via click - headers are
/// unselectable, see `bind()` in `window.rs` - but kept as a defensive
/// no-op case), a single message drives it, and two or more messages show
/// a "N selected" placeholder instead - see the selection handler in
/// `window.rs`.
pub enum SelectionKind {
    Empty,
    // Never constructed by `selection_kind()` today - headers are
    // unselectable, so no click/ctrl-click/shift-click can ever produce
    // this - but kept as a real variant, not deleted, so the window.rs
    // selection handler's defensive no-op arm for it has something to match
    // against if a header is ever selected some other way (e.g. a future
    // programmatic `select_item` on a header's row index).
    #[allow(dead_code)]
    Section,
    Message(Box<EmailSummary>),
    Multiple(Vec<EmailSummary>),
}

/// The message list's model: a `Gtk.TreeListModel` whose root rows are
/// collapsible date sections and whose children are either the messages in
/// each (unthreaded) or collapsible conversation headers whose own children
/// are their messages (threaded), plus the selection over it. Replaces what
/// used to be a flat `gio::ListStore` of messages paired with a
/// `SingleSelection`.
///
/// The selection is a `MultiSelection` (not `SingleSelection`) so batch
/// actions can act on more than one message - `GtkListView` handles
/// ctrl/shift-click natively once bound to a multi-selection model, and the
/// per-row checkbox in `window.rs` toggles the same underlying selection
/// through the ordinary `GtkSelectionModel` interface. Unlike
/// `SingleSelection`, `MultiSelection` never autoselects row 0 on a rebuild -
/// see `restore_selection`'s doc for what that means for this model.
///
/// Cloning is cheap and shares the same underlying model - handlers close
/// over clones freely, exactly as they did over the old store/selection pair.
#[derive(Clone)]
pub struct MessageListModel {
    /// The tree's root level: `Section` rows when grouped, `Message` rows
    /// when flat.
    root: gio::ListStore,
    tree: gtk::TreeListModel,
    pub selection: gtk::MultiSelection,
    /// One child store per bucket, handed out by the tree's create-child
    /// closure. These must outlive any individual rebuild: a `TreeListRow`
    /// that survives a `splice` still holds the child model it was given, so
    /// swapping in a fresh store would leave that row pointing at a discarded
    /// one. Rebuilds splice *into* these instead.
    sections: Rc<RefCell<HashMap<DateBucket, gio::ListStore>>>,
    /// One child store per conversation, for the thread rows' own children -
    /// the third tree level, only populated while threading is on. Same
    /// outlive-the-rebuild discipline as `sections`.
    threads: Rc<RefCell<HashMap<ThreadId, gio::ListStore>>>,
    /// Sections the user has collapsed. Collapse is the exception, so an
    /// absent bucket means expanded and a newly-appearing section defaults
    /// open. Lives outside the model so it survives the constant rebuilds
    /// (cache replay, live sync, on-demand sync) that recreate every row.
    collapsed: Rc<RefCell<HashSet<DateBucket>>>,
    /// Conversations the user has collapsed, keyed by the thread's stable
    /// `(mailbox, thread key)` identity - the same discipline as `collapsed`,
    /// and the reason `ThreadRow` keeps its id rather than deriving anything
    /// from display data. Kept even across threading-mode toggles (a capture
    /// only rewrites it when thread rows were actually on screen), so
    /// off-and-on doesn't forget the user's collapses.
    collapsed_threads: Rc<RefCell<HashSet<ThreadId>>>,
    /// Whether conversations render collapsed under their headers (on) or as
    /// bare messages (off). Owned by the model rather than `UiState`, like
    /// `filter`, so the rebuild-skipping check in `repopulate` can compare it
    /// without threading it through every call site; `set_threaded` is the
    /// one way in.
    threaded: Rc<RefCell<bool>>,
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
        let threads: Rc<RefCell<HashMap<ThreadId, gio::ListStore>>> = Rc::new(RefCell::new(HashMap::new()));
        let tree = gtk::TreeListModel::new(root.clone(), false, false, {
            let sections = sections.clone();
            let threads = threads.clone();
            move |item| {
                let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
                let item = boxed.borrow::<MessageItem>();
                let store = match &*item {
                    // A section's children are its conversation headers and
                    // lone messages.
                    MessageItem::Section(section) => sections
                        .borrow_mut()
                        .entry(section.bucket)
                        .or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
                        .clone(),
                    // A conversation's children are its messages.
                    MessageItem::Thread(thread) => threads
                        .borrow_mut()
                        .entry(thread.id.clone())
                        .or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
                        .clone(),
                    // A message is a leaf: no child model, no expander.
                    MessageItem::Message(_) => return None,
                };
                Some(store.upcast::<gio::ListModel>())
            }
        });
        // `passthrough = false` (as in the folder tree) means the selection
        // and the row factory see `Gtk.TreeListRow`s, which is what drives
        // the section expanders. `autoexpand = false` because expansion is
        // driven from `collapsed`, not left to the model.
        let selection = gtk::MultiSelection::new(Some(tree.clone()));
        MessageListModel {
            root,
            tree,
            selection,
            sections,
            threads,
            collapsed: Rc::new(RefCell::new(HashSet::new())),
            collapsed_threads: Rc::new(RefCell::new(HashSet::new())),
            threaded: Rc::new(RefCell::new(true)),
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

    /// The selected row's message, or `None` when nothing is selected, the
    /// selection has landed on a section header, or more than one message is
    /// selected.
    pub fn selected_summary(&self) -> Option<EmailSummary> {
        match self.selection_kind() {
            SelectionKind::Message(summary) => Some(*summary),
            _ => None,
        }
    }

    /// Every message currently selected, in ascending row-position order.
    /// Empty when nothing is selected; a lone section header can never
    /// appear here since headers are unselectable (see `bind()` in
    /// `window.rs`).
    pub fn selected_summaries(&self) -> Vec<EmailSummary> {
        let bitset = self.selection.selection();
        let mut out = Vec::with_capacity(bitset.size() as usize);
        if let Some((mut iter, first)) = gtk::BitsetIter::init_first(&bitset) {
            let mut pos = Some(first);
            while let Some(p) = pos {
                if let Some(summary) = self.summary_at(p) {
                    out.push(summary);
                }
                pos = iter.next();
            }
        }
        out
    }

    pub fn selection_kind(&self) -> SelectionKind {
        let mut summaries = self.selected_summaries();
        match summaries.len() {
            0 => SelectionKind::Empty,
            1 => SelectionKind::Message(Box::new(summaries.remove(0))),
            _ => SelectionKind::Multiple(summaries),
        }
    }

    /// The full `EmailSummary` at flat position `i`, or `None` if that row is
    /// a section or thread header. Like `message_at`, but cloning the whole
    /// summary rather than just its `(mailbox, uid)` identity - `message_at`
    /// stays separate since `restore_selection`'s identity-only walk
    /// shouldn't pay for a clone it doesn't need.
    fn summary_at(&self, i: u32) -> Option<EmailSummary> {
        let row = self.selection.item(i).and_downcast::<gtk::TreeListRow>()?;
        let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let item = boxed.borrow::<MessageItem>();
        match &*item {
            MessageItem::Message(summary) => Some((**summary).clone()),
            MessageItem::Section(_) | MessageItem::Thread(_) => None,
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
    /// set several times in a row, and rebuilding for each one would tear
    /// down and rebuild every row for nothing, needlessly re-running
    /// `restore_selection` below and risking a spurious flicker in the
    /// reading pane even though the selection ultimately restores to the
    /// same place.
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
        // subset happens to be element-identical. And so is the threading
        // mode: the same message set renders a different tree under
        // conversations than under bare messages, and flipping the View-tab
        // toggle must rebuild even when nothing else moved.
        let threaded = *self.threaded.borrow();
        let unchanged = self.displayed.borrow().as_ref().is_some_and(|(key, descending, shown_filter, shown_threaded, current)| {
            *key == sort_key
                && *descending == sort_descending
                && *shown_filter == filter
                && *shown_threaded == threaded
                && current.len() == messages.len()
                && current.iter().zip(messages.iter()).all(|(a, b)| message_row_key(a) == message_row_key(b))
        });
        if unchanged {
            return;
        }

        // Snapshot what the user has collapsed before anything is spliced.
        self.capture_collapsed();

        // Remember the current highlight(s) before the rebuild wipes them:
        // the splices drop the old row objects, which would otherwise lose
        // the user's selection every time a delete/archive/snooze lands a
        // `MessagesUpdated`. The single previously-focused position is kept
        // separately (only meaningful when exactly one row was selected) as
        // the N=1 fallback `restore_selection` uses when that one row didn't
        // survive the rebuild.
        let bitset = self.selection.selection();
        let previous_selection: Vec<(MailboxId, Uid)> = self.selected_summaries().iter().map(|s| (s.mailbox.clone(), s.uid)).collect();
        let previous_single_index = if bitset.size() == 1 {
            gtk::BitsetIter::init_first(&bitset).map(|(_, first)| first).unwrap_or(gtk::INVALID_LIST_POSITION)
        } else {
            gtk::INVALID_LIST_POSITION
        };

        *self.displayed.borrow_mut() = Some((sort_key, sort_descending, filter, threaded, messages.clone()));

        match build_layout(messages, sort_key, Local::now(), threaded, sort_descending) {
            ListLayout::Flat(messages) => {
                // No sections, no threads: every child store drains, so a
                // later threaded rebuild can't resurrect rows from a layout
                // that has been gone since the last capture.
                self.clear_sections(&HashSet::new());
                self.clear_threads(&HashSet::new());
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
                // A grouped layout holds no thread rows, so every thread store
                // drains too - stale threads must not survive into a rebuild
                // that re-threads and finds the same id again.
                self.clear_threads(&HashSet::new());

                let rows: Vec<glib::Object> = sections
                    .into_iter()
                    .map(|(bucket, label, _)| glib::BoxedAnyObject::new(MessageItem::Section(SectionRow { bucket, label })).upcast())
                    .collect();
                self.root.splice(0, self.root.n_items(), &rows);
                self.apply_expansion();
            }
            ListLayout::Threaded(sections) => {
                // Two child levels to splice before the roots: each section's
                // store holds its conversation headers and lone messages, and
                // each conversation's store holds its messages.
                let live_buckets: HashSet<DateBucket> = sections.iter().map(|(b, _, _)| *b).collect();
                let mut live_threads: HashSet<ThreadId> = HashSet::new();
                for (bucket, _, entries) in &sections {
                    let store = self
                        .sections
                        .borrow_mut()
                        .entry(*bucket)
                        .or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
                        .clone();
                    let rows: Vec<glib::Object> = entries
                        .iter()
                        .map(|entry| match entry {
                            ThreadEntry::Thread(thread) => glib::BoxedAnyObject::new(MessageItem::Thread(thread.row.clone())).upcast(),
                            ThreadEntry::Single(message) => glib::BoxedAnyObject::new(MessageItem::Message(Box::new(message.clone()))).upcast(),
                        })
                        .collect();
                    store.splice(0, store.n_items(), &rows);

                    for entry in entries {
                        let ThreadEntry::Thread(thread) = entry else { continue };
                        live_threads.insert(thread.row.id.clone());
                        let thread_store = self
                            .threads
                            .borrow_mut()
                            .entry(thread.row.id.clone())
                            .or_insert_with(gio::ListStore::new::<glib::BoxedAnyObject>)
                            .clone();
                        // Children oldest-first (the order `group_threads`
                        // left them in) - the natural reading order of a
                        // conversation regardless of the list's sort.
                        let message_rows: Vec<glib::Object> = thread
                            .messages
                            .iter()
                            .map(|m| glib::BoxedAnyObject::new(MessageItem::Message(Box::new(m.clone()))).upcast())
                            .collect();
                        thread_store.splice(0, thread_store.n_items(), &message_rows);
                    }
                }
                self.clear_sections(&live_buckets);
                self.clear_threads(&live_threads);

                let rows: Vec<glib::Object> = sections
                    .into_iter()
                    .map(|(bucket, label, _)| glib::BoxedAnyObject::new(MessageItem::Section(SectionRow { bucket, label })).upcast())
                    .collect();
                self.root.splice(0, self.root.n_items(), &rows);
                self.apply_expansion();
            }
        }

        self.restore_selection(previous_selection, previous_single_index);
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
            Some((key, descending, _, _, _)) => (key, descending),
            // Nothing has ever been rendered; the defaults are what a first
            // repopulate would use anyway.
            None => (SortKey::Date, true),
        };
        let truth = self.all_messages();
        self.repopulate(truth, sort_key, sort_descending);
    }

    /// Turns conversation grouping on or off and re-renders from the stored
    /// unfiltered set. Only a `SortKey::Date` list is affected - sender and
    /// subject sorts render flat either way (`build_layout`), so toggling
    /// while one is active just records the mode for when the list is next
    /// sorted by date. A no-op when the mode is already in effect.
    pub fn set_threaded(&self, threaded: bool) {
        if *self.threaded.borrow() == threaded {
            return;
        }
        *self.threaded.borrow_mut() = threaded;
        let (sort_key, sort_descending) = match *self.displayed.borrow() {
            Some((key, descending, _, _, _)) => (key, descending),
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
            Some((key, descending, _, _, _)) => (key, descending),
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

    /// Empties every conversation store whose thread id isn't in `live` - the
    /// `sections`-level counterpart for the threads map. Called whenever the
    /// new layout holds no thread rows at all (flat and grouped layouts pass
    /// an empty set), so a stale thread store can never resurface its old
    /// messages under a row the next threaded rebuild re-creates for the same
    /// id.
    fn clear_threads(&self, live: &HashSet<ThreadId>) {
        let stale: Vec<gio::ListStore> = self.threads.borrow().iter().filter(|(id, _)| !live.contains(*id)).map(|(_, store)| store.clone()).collect();
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
        let collapsed_threads = self.collapsed_threads.borrow().clone();
        for i in 0..self.root.n_items() {
            // `child_row` indexes the *root* model, so this walk is immune to
            // the flat row count changing underneath it as each expansion
            // splices children in. Iterating `tree.n_items()` would not be.
            let Some(row) = self.tree.child_row(i) else { continue };
            let Some(bucket) = section_bucket(&row) else { continue };
            row.set_expanded(!collapsed.contains(&bucket));
            // One level deeper: each section's children include conversation
            // headers, which get their own expansion state. `row.child_row`
            // is the tree's own child accessor (the section must be expanded
            // first, hence the order above); `row.children()` would hand back
            // the *raw* child store - `BoxedAnyObject` items, not
            // `TreeListRow`s - and a thread collapsed by the user would then
            // silently re-expand here.
            if row.is_expanded() {
                let mut c = 0;
                while let Some(child) = row.child_row(c) {
                    c += 1;
                    if let Some(id) = thread_id(&child) {
                        child.set_expanded(!collapsed_threads.contains(&id));
                    }
                }
            }
        }
    }

    /// Records which sections - and, one level deeper, which conversations -
    /// are collapsed right now, so the next rebuild can put them back. Must
    /// run before any splice - the rows it reads are exactly what the splice
    /// destroys.
    ///
    /// Reading the model beats listening to `notify::expanded`, which was the
    /// obvious approach and does not work: `TreeListModel` hands out
    /// `TreeListRow`s on demand and doesn't retain them, so a handler
    /// connected during a rebuild is dropped with its row and never sees the
    /// user's later collapse. The same signal also fires when a splice tears
    /// rows down, which is indistinguishable from a real collapse. The
    /// model's own expansion state has neither problem.
    ///
    /// A thread row nested under a *collapsed* section is invisible, so it
    /// can never have been collapsed by the user and reads as expanded
    /// whatever the model materialized - capturing that as "not collapsed" is
    /// exactly right.
    fn capture_collapsed(&self) {
        let mut collapsed = HashSet::new();
        let mut collapsed_threads = HashSet::new();
        let mut saw_section = false;
        let mut saw_thread = false;
        for i in 0..self.root.n_items() {
            let Some(row) = self.tree.child_row(i) else { continue };
            let Some(bucket) = section_bucket(&row) else { continue };
            saw_section = true;
            if !row.is_expanded() {
                collapsed.insert(bucket);
            }
            // Only an expanded section's thread rows can carry a user's
            // collapse - a collapsed one hides its threads from interaction.
            if row.is_expanded() {
                let mut c = 0;
                while let Some(child) = row.child_row(c) {
                    c += 1;
                    if let Some(id) = thread_id(&child) {
                        saw_thread = true;
                        if !child.is_expanded() {
                            collapsed_threads.insert(id);
                        }
                    }
                }
            }
        }
        // Only a grouped list can speak for the collapsed sets. A flat one
        // (a sender/subject sort) has no section or thread rows at all, and
        // letting it write empty sets would forget the user's collapses
        // across a there-and-back sort change. Each set is only rewritten
        // when rows of its own kind were on screen, so an unthreaded capture
        // never wipes the thread collapses (or vice versa).
        if saw_section {
            *self.collapsed.borrow_mut() = collapsed;
        }
        if saw_thread {
            *self.collapsed_threads.borrow_mut() = collapsed_threads;
        }
    }

    /// Restores the highlight(s) after a rebuild: every previously-selected
    /// message that's still present (the rebuild wasn't a delete of it),
    /// plus - only when exactly one row was selected before and it didn't
    /// survive - whatever now occupies its old spot. There is no equivalent
    /// fallback for a multi-message selection that didn't survive at all:
    /// unlike a single highlight, there's no one "next best" row to guess at
    /// for several deleted messages, so the selection legitimately ends up
    /// empty (the reading pane's `Empty` arm handles that correctly, unlike
    /// `GtkSingleSelection`'s old autoselect-row-0 behavior, which this model
    /// doesn't have - see `MessageListModel`'s doc).
    ///
    /// Every survivor is applied in one `set_selection` call rather than a
    /// loop of `select_item` calls, so a rebuild that restores N previously-
    /// selected rows fires `selection-changed` once, not N times - the
    /// selection handler in `window.rs` re-derives its state from scratch
    /// each time it fires, so N firings would re-run that logic (and briefly
    /// flash through intermediate partial-selection states) for no reason.
    fn restore_selection(&self, previous: Vec<(MailboxId, Uid)>, previous_single_index: u32) {
        let n = self.selection.n_items();
        if n == 0 {
            return;
        }
        let previous: HashSet<(MailboxId, Uid)> = previous.into_iter().collect();
        let survivors = gtk::Bitset::new_empty();
        let mut survivor_count = 0u32;
        for i in 0..n {
            if let Some((m, u)) = self.message_at(i) {
                if previous.contains(&(m, u)) {
                    survivors.add(i);
                    survivor_count += 1;
                }
            }
        }
        if survivor_count > 0 {
            self.selection.set_selection(&survivors, &gtk::Bitset::new_range(0, n));
            return;
        }
        if previous.len() != 1 || previous_single_index == gtk::INVALID_LIST_POSITION {
            // Either nothing was selected before, or several messages were
            // and none survived - there's no single fallback position that
            // makes sense for a vanished multi-selection (see doc above).
            return;
        }
        // Exactly one row was selected before and it's gone: preserve the
        // old single-selection "highlight follows deletion" UX by falling
        // back to whatever now occupies its old position, walking forward
        // past any section header so the highlight never parks on one.
        let mut index = previous_single_index.min(n - 1);
        while index < n && self.message_at(index).is_none() {
            index += 1;
        }
        if index < n {
            self.selection.select_item(index, true);
        }
    }

    /// The `(mailbox, uid)` of the message at flat position `i`, or `None`
    /// if that row is a section or thread header.
    fn message_at(&self, i: u32) -> Option<(MailboxId, Uid)> {
        let row = self.selection.item(i).and_downcast::<gtk::TreeListRow>()?;
        let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
        let item = boxed.borrow::<MessageItem>();
        match &*item {
            MessageItem::Message(summary) => Some((summary.mailbox.clone(), summary.uid)),
            MessageItem::Section(_) | MessageItem::Thread(_) => None,
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
        MessageItem::Message(_) | MessageItem::Thread(_) => None,
    }
}

/// The thread identity a section-level `TreeListRow` heads, or `None` if the
/// row is a section header or a message.
fn thread_id(row: &gtk::TreeListRow) -> Option<ThreadId> {
    let boxed = row.item().and_downcast::<glib::BoxedAnyObject>()?;
    let item = boxed.borrow::<MessageItem>();
    match &*item {
        MessageItem::Thread(thread) => Some(thread.id.clone()),
        MessageItem::Section(_) | MessageItem::Message(_) => None,
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

    /// Mail older than a year is grouped by its own calendar year rather than
    /// lumped under "Older". The cutoff is by *age* (12 months), so a
    /// December 2025 mail still reads "Older" in August 2026 while a July
    /// 2025 mail is already "2025" - and the year is fixed for good, so a
    /// collapse recorded against `Year(2024)` never goes stale.
    #[test]
    fn mail_older_than_a_year_is_grouped_by_its_own_year() {
        let now = local(2026, 8, 4, 8);
        // Exactly a year back is not *older* than a year; one day more is.
        assert_eq!(bucket_for(utc_on(now, 2025, 8, 4), now), DateBucket::Older, "exactly 12 months old stays Older");
        assert_eq!(bucket_for(utc_on(now, 2025, 8, 3), now), DateBucket::Year(2025));
        assert_eq!(bucket_for(utc_on(now, 2025, 7, 15), now), DateBucket::Year(2025));
        assert_eq!(bucket_label(DateBucket::Year(2025), now), "2025");
        assert_eq!(bucket_for(utc_on(now, 2024, 12, 25), now), DateBucket::Year(2024));
        assert_eq!(bucket_for(utc_on(now, 2023, 1, 1), now), DateBucket::Year(2023));
        // Eleven months back is still "Older", not "2025".
        assert_eq!(bucket_for(utc_on(now, 2025, 9, 4), now), DateBucket::Older);
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

        // --- Conversations: a thread of two collapses under one header, and
        // the collapse survives both rebuilds and a threading-mode round-trip
        // ---
        let threaded = MessageListModel::build();
        // The reply is today's, the root yesterday's - the thread's section is
        // the *newest* member's, so only Today remains.
        let root = in_thread(summary(1, yesterday), "t");
        let reply = in_thread(summary(2, today), "t");
        let single = summary(3, today);
        threaded.repopulate(vec![reply, root, single], SortKey::Date, true);
        // Today's header + thread header + its 2 children + the single.
        assert_eq!(threaded.selection.n_items(), 5, "the thread did not expand");
        assert!(threaded.collapsed_threads.borrow().is_empty(), "a rebuild recorded a phantom thread collapse");

        threaded.tree.child_row(0).expect("root row 0").set_expanded(true);
        let thread_row = threaded.tree.child_row(0).expect("root row 0").child_row(0).expect("thread row");
        assert!(thread_row.is_expandable(), "the thread header is not a collapsible row");
        thread_row.set_expanded(false);
        assert_eq!(threaded.selection.n_items(), 3, "collapsing the thread hid its messages");

        // A rebuild with different contents can't short-circuit the identity
        // check - the thread collapse must survive it.
        threaded.repopulate(
            vec![in_thread(summary(4, today), "t"), in_thread(summary(1, yesterday), "t"), summary(3, today)],
            SortKey::Date,
            true,
        );
        let thread_row = threaded.tree.child_row(0).expect("root row 0").child_row(0).expect("thread row");
        assert!(!thread_row.is_expanded(), "the thread collapse did not survive a rebuild");

        // Toggling conversations off flattens the thread back into messages,
        // and back on re-groups it - with the collapse remembered.
        threaded.set_threaded(false);
        assert_eq!(threaded.selection.n_items(), 5, "unthreaded: two sections + three messages");
        threaded.set_threaded(true);
        assert_eq!(threaded.selection.n_items(), 3, "threading back on must not expand a collapsed thread");
        let thread_row = threaded.tree.child_row(0).expect("root row 0").child_row(0).expect("thread row");
        assert!(!thread_row.is_expanded(), "the collapse was forgotten across the mode toggle");

        // --- A threading toggle is a real change even when the message set
        // is byte-identical, so the no-op check can't swallow the rebuild ---
        threaded.set_threaded(false);
        threaded.set_threaded(true);
        assert_eq!(threaded.selection.n_items(), 3, "flipping the mode did not rebuild");

        // --- A sender sort ignores threading and renders flat ---
        threaded.repopulate(vec![summary(1, today), summary(2, today)], SortKey::Sender, true);
        assert_eq!(threaded.selection.n_items(), 2, "a sender sort should render flat under threading");
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
            has_calendar: false,
            preview: None,
            structure: None,
        }
    }

    #[test]
    fn non_date_sorts_render_flat() {
        let now = local(2026, 8, 4, 8);
        let messages = vec![summary(1, utc_on(now, 2026, 8, 4)), summary(2, utc_on(now, 2026, 2, 1))];
        assert!(matches!(
            build_layout(messages.clone(), SortKey::Sender, now, true, true),
            ListLayout::Flat(m) if m.len() == 2
        ));
        assert!(matches!(
            build_layout(messages, SortKey::Subject, now, true, true),
            ListLayout::Flat(m) if m.len() == 2
        ));
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
        let ListLayout::Grouped(sections) = build_layout(messages, SortKey::Date, now, false, true) else {
            panic!("expected a grouped layout");
        };
        let shape: Vec<(DateBucket, &str, usize)> = sections.iter().map(|(b, l, m)| (*b, l.as_str(), m.len())).collect();
        assert_eq!(
            shape,
            vec![(DateBucket::Today, "Today", 2), (DateBucket::LastMonth, "July", 1), (DateBucket::Older, "Older", 1)]
        );
    }

    /// Year headers cut as their own consecutive runs, newest year first -
    /// the same run-cutting that makes the recent sections fall out in order.
    #[test]
    fn year_buckets_cut_one_section_per_year() {
        let now = local(2026, 8, 4, 8);
        let messages = vec![
            summary(1, utc_on(now, 2025, 7, 15)),
            summary(2, utc_on(now, 2025, 3, 10)),
            summary(3, utc_on(now, 2024, 12, 25)),
            summary(4, utc_on(now, 2023, 1, 1)),
        ];
        let ListLayout::Grouped(sections) = build_layout(messages, SortKey::Date, now, false, true) else {
            panic!("expected a grouped layout");
        };
        let shape: Vec<(DateBucket, &str, usize)> = sections.iter().map(|(b, l, m)| (*b, l.as_str(), m.len())).collect();
        assert_eq!(
            shape,
            vec![
                (DateBucket::Year(2025), "2025", 2),
                (DateBucket::Year(2024), "2024", 1),
                (DateBucket::Year(2023), "2023", 1),
            ]
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
        let ListLayout::Grouped(sections) = build_layout(messages, SortKey::Date, now, false, false) else {
            panic!("expected a grouped layout");
        };
        let buckets: Vec<DateBucket> = sections.iter().map(|(b, _, _)| *b).collect();
        assert_eq!(buckets, vec![DateBucket::Older, DateBucket::LastMonth, DateBucket::Today]);
    }

    fn in_thread(mut summary: EmailSummary, key: &str) -> EmailSummary {
        summary.thread_key = ThreadKey(key.into());
        summary
    }

    fn from_alice(mut summary: EmailSummary) -> EmailSummary {
        summary.from = vec![lookout_core::EmailAddress {
            name: Some("Alice".to_string()),
            address: "alice@example.com".to_string(),
        }];
        summary
    }

    /// The latest message a thread header's `messages` vec ends with -
    /// `messages` is oldest-first, so the newest member is its last element.
    fn latest_uid(entry: &ThreadEntry) -> u32 {
        match entry {
            ThreadEntry::Thread(t) => t.messages.last().map(|m| m.uid.0).unwrap_or(t.row.latest.timestamp() as u32),
            ThreadEntry::Single(m) => m.uid.0,
        }
    }

    fn shape(sections: &[(DateBucket, String, Vec<ThreadEntry>)]) -> Vec<(DateBucket, &str, Vec<&str>)> {
        sections
            .iter()
            .map(|(b, l, entries)| {
                let kinds = entries.iter().map(|e| match e {
                    ThreadEntry::Thread(t) => t.row.subject.as_deref().unwrap_or("(no subject)"),
                    ThreadEntry::Single(m) => m.subject.as_deref().unwrap_or("(no subject)"),
                });
                (*b, l.as_str(), kinds.collect())
            })
            .collect()
    }

    /// A two-message conversation (root + reply) collapses under one thread
    /// header, which sits in the section of its *newest* message - the reply
    /// landing today pulls the whole thread (two-day-old root included) up
    /// into Today. A lone message stays a bare row.
    #[test]
    fn threaded_layout_buckets_by_the_newest_message_and_keeps_singletons_flat() {
        let now = local(2026, 8, 4, 8);
        let root = with_subject(from_alice(in_thread(summary(1, utc_on(now, 2026, 8, 2)), "kickoff")), "Project kickoff");
        let reply = with_subject(from_alice(in_thread(summary(2, utc_on(now, 2026, 8, 4)), "kickoff")), "Re: Project kickoff");
        let single = with_subject(summary(3, utc_on(now, 2026, 7, 15)), "Standalone");
        let messages = vec![reply.clone(), single.clone(), root.clone()];
        let ListLayout::Threaded(sections) = build_layout(messages, SortKey::Date, now, true, true) else {
            panic!("expected a threaded layout");
        };
        // The thread's root stays inside it (not a second Today row) and the
        // thread itself sits in the bucket of its newest member.
        assert_eq!(
            shape(&sections),
            vec![
                (DateBucket::Today, "Today", vec!["Re: Project kickoff"]),
                (DateBucket::LastMonth, "July", vec!["Standalone"])
            ]
        );
        let (_, _, today) = &sections[0];
        match &today[0] {
            ThreadEntry::Thread(thread) => {
                assert_eq!(thread.row.count, 2);
                assert_eq!(thread.row.subject.as_deref(), Some("Re: Project kickoff"));
                // Oldest-first reading order inside the conversation.
                assert_eq!(thread.messages.iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![1, 2]);
                // The sender and aggregate flags follow the newest message
                // (the test fixtures are unread by default, so the header
                // inherits that from its newest member).
                assert_eq!(thread.row.sender, "Alice");
                assert!(thread.row.has_unread);
            }
            ThreadEntry::Single(_) => panic!("the conversation should be a thread header"),
        }
    }

    /// An ascending date sort mirrors the sections and the threads within
    /// them, but a conversation's own message order stays oldest-first - the
    /// natural reading order, whatever direction the list sorts.
    #[test]
    fn threaded_layout_mirrors_sections_not_thread_contents() {
        let now = local(2026, 8, 4, 8);
        // Two threads, both with replies today; the earlier-started one is
        // also the one with the older latest reply.
        let older_thread = {
            let root = in_thread(summary(1, utc_on(now, 2026, 8, 1)), "a");
            let reply = in_thread(summary(2, utc_on(now, 2026, 8, 4) + chrono::Duration::hours(9)), "a");
            vec![root, reply]
        };
        let newer_thread = {
            let root = in_thread(summary(3, utc_on(now, 2026, 8, 2)), "b");
            let reply = in_thread(summary(4, utc_on(now, 2026, 8, 4) + chrono::Duration::hours(11)), "b");
            vec![root, reply]
        };
        let mut descending = older_thread.clone();
        descending.extend(newer_thread.iter().cloned());
        sort_messages(&mut descending, SortKey::Date, true);
        let ListLayout::Threaded(desc) = build_layout(descending, SortKey::Date, now, true, true) else {
            panic!("expected a threaded layout");
        };
        let (_, _, entries) = &desc[0];
        let desc_order: Vec<u32> = entries.iter().map(latest_uid).collect();
        assert_eq!(desc_order, vec![4, 2], "newest latest-reply first");

        let mut ascending = older_thread;
        ascending.extend(newer_thread);
        // The ascending caller passes the list oldest-first, matching what
        // `sort_messages` produces for `descending = false`.
        sort_messages(&mut ascending, SortKey::Date, false);
        let ListLayout::Threaded(asc) = build_layout(ascending, SortKey::Date, now, true, false) else {
            panic!("expected a threaded layout");
        };
        let (_, _, entries) = &asc[0];
        let asc_order: Vec<u32> = entries.iter().map(latest_uid).collect();
        assert_eq!(asc_order, vec![2, 4], "oldest latest-reply first when ascending");
        let ThreadEntry::Thread(first) = &entries[0] else { panic!("expected a thread") };
        assert_eq!(first.messages.iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![1, 2], "thread contents never mirror");
    }

    /// Two mailboxes can produce identical `ThreadKey`s (the subject-fallback
    /// keys in particular), and the unified view merges both mailboxes into
    /// one list - so the thread identity must be mailbox-prefixed or the
    /// unrelated messages would merge into one conversation.
    #[test]
    fn thread_identity_is_mailbox_scoped() {
        let now = local(2026, 8, 4, 8);
        let mut a = with_subject(in_thread(at(1, "one:INBOX", 2026, 8, 4, 9), "weekly"), "Weekly sync");
        let mut b = with_subject(in_thread(at(2, "two:INBOX", 2026, 8, 4, 9), "weekly"), "Weekly sync");
        a.message_id = Some("<a@x>".into());
        b.message_id = Some("<b@x>".into());
        let ListLayout::Threaded(sections) = build_layout(vec![a, b], SortKey::Date, now, true, true) else {
            panic!("expected a threaded layout");
        };
        let (_, _, entries) = &sections[0];
        assert_eq!(entries.len(), 2, "same subject-fallback key in two mailboxes must not merge");
        assert!(entries.iter().all(|e| matches!(e, ThreadEntry::Single(_))));
    }

    fn with_subject(mut summary: EmailSummary, subject: &str) -> EmailSummary {
        summary.subject = Some(subject.to_string());
        summary
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
