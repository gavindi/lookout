//! The single-line message list's columns (OUTLOOK_PARITY.md M-07 / UX-27).
//!
//! Outlook names its columns - Importance, Attachment, From, Subject, Received,
//! Size, Categories, Flag - and makes each header clickable to sort by that
//! field. This module is the pure half of that: the column set, the sort key
//! each column drives, the width it asks for, how a stored column list parses,
//! and how the set narrows when the list pane is too narrow to show it all.
//! The widget wiring lives in `window/columns.rs`; the rows read the visible
//! set from there.
//!
//! Outlook's decorative "Icon" column (the item-type picture) has no
//! equivalent here: the row already carries the attachment and meeting icons
//! inline, and `EmailSummary` has no message-class field to sort on. It is
//! deliberately left out rather than shown as a header that cannot sort.
//!
//! Copyright (C) <2026>  <Gavin Graham & Contributors>
//! Software released under the GPL3 license

use crate::message_list::SortKey;

/// One column of the single-line message list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageColumn {
    Importance,
    Attachment,
    From,
    To,
    Subject,
    Received,
    Size,
    Categories,
    Flag,
}

impl MessageColumn {
    /// Every column, in the order the column chooser lists them.
    pub const ALL: [MessageColumn; 9] = [
        MessageColumn::Importance,
        MessageColumn::Attachment,
        MessageColumn::From,
        MessageColumn::To,
        MessageColumn::Subject,
        MessageColumn::Received,
        MessageColumn::Size,
        MessageColumn::Categories,
        MessageColumn::Flag,
    ];

    /// The stable id stored in `settings.json` / GSettings and used by the
    /// header's actions. Never rename one of these - it would forget the
    /// user's column choice.
    pub fn id(self) -> &'static str {
        match self {
            MessageColumn::Importance => "importance",
            MessageColumn::Attachment => "attachment",
            MessageColumn::From => "from",
            MessageColumn::To => "to",
            MessageColumn::Subject => "subject",
            MessageColumn::Received => "received",
            MessageColumn::Size => "size",
            MessageColumn::Categories => "categories",
            MessageColumn::Flag => "flag",
        }
    }

    /// The header button's text - Outlook's column names.
    pub fn label(self) -> &'static str {
        match self {
            MessageColumn::Importance => "Importance",
            MessageColumn::Attachment => "Attachment",
            MessageColumn::From => "From",
            MessageColumn::To => "To",
            MessageColumn::Subject => "Subject",
            MessageColumn::Received => "Received",
            MessageColumn::Size => "Size",
            MessageColumn::Categories => "Categories",
            MessageColumn::Flag => "Flag",
        }
    }

    /// The message-list sort this column drives. The list's names differ a
    /// little from the column headings ("From" is `SortKey::Sender`,
    /// "Received" is `SortKey::Date`, "Categories" is `SortKey::Category`).
    pub fn sort_key(self) -> SortKey {
        match self {
            MessageColumn::Importance => SortKey::Importance,
            MessageColumn::Attachment => SortKey::Attachments,
            MessageColumn::From => SortKey::Sender,
            MessageColumn::To => SortKey::To,
            MessageColumn::Subject => SortKey::Subject,
            MessageColumn::Received => SortKey::Date,
            MessageColumn::Size => SortKey::Size,
            MessageColumn::Categories => SortKey::Category,
            MessageColumn::Flag => SortKey::Flag,
        }
    }

    /// The width this column's header and row cell request, in pixels. The
    /// subject is the only expanding column and requests nothing.
    pub fn width(self) -> i32 {
        match self {
            MessageColumn::Importance => 24,
            MessageColumn::Attachment => 28,
            MessageColumn::From => 170,
            MessageColumn::To => 150,
            MessageColumn::Subject => 0,
            MessageColumn::Received => 92,
            MessageColumn::Size => 64,
            MessageColumn::Categories => 110,
            MessageColumn::Flag => 28,
        }
    }

    /// Whether the column takes the row's slack (Subject only).
    pub fn expands(self) -> bool {
        matches!(self, MessageColumn::Subject)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|column| column.id() == id)
    }
}

/// Outlook's default single-line columns, in its order. "To" is offered by the
/// column chooser but hidden by default, as in Outlook.
pub const DEFAULT_COLUMNS: [MessageColumn; 8] = [
    MessageColumn::Importance,
    MessageColumn::Attachment,
    MessageColumn::From,
    MessageColumn::Subject,
    MessageColumn::Received,
    MessageColumn::Size,
    MessageColumn::Categories,
    MessageColumn::Flag,
];

/// The gutter left of the first column: the unread accent bar (3 px) plus the
/// avatar cell's two 8 px margins and its 32 px circle. The header uses the
/// same number so its labels sit exactly over their cells.
pub const ROW_LEADING_WIDTH: i32 = 51;

/// The unread accent bar's width; the rest of [`ROW_LEADING_WIDTH`] is the
/// avatar/checkbox slot the rows and the header share.
pub const UNREAD_ACCENT_WIDTH: i32 = 3;

/// The gap between two column cells, used by both the rows and the header and
/// by every width calculation, so the automatic layout threshold and the
/// fitting rule can never drift from what is actually drawn.
pub const COLUMN_SPACING: i32 = 8;

/// The margin after the last column, in the rows and the header.
pub const ROW_TRAILING_MARGIN: i32 = 10;

/// The width the Subject column is allowed at minimum when deciding whether a
/// column set fits; the flexible cell itself may take more.
pub const SUBJECT_MIN_WIDTH: i32 = 90;

/// The width one single-line row needs for `columns`: the leading gutter, the
/// column cells (with the Subject at its minimum), the gaps between them and
/// the trailing margin. This is the one width formula the column fitting and
/// the automatic layout threshold share.
pub fn row_width(columns: &[MessageColumn]) -> i32 {
    let cells: i32 = columns.iter().map(|column| if column.expands() { SUBJECT_MIN_WIDTH } else { column.width() }).sum();
    ROW_LEADING_WIDTH + ROW_TRAILING_MARGIN + cells + COLUMN_SPACING * (columns.len().saturating_sub(1) as i32)
}

/// The width at which Outlook's default column set fits. The automatic layout
/// switches to the single-line list at or above this width; it is derived
/// from the column model, so changing a column width or the spacing moves the
/// threshold with it.
pub fn preferred_single_line_width() -> i32 {
    row_width(&DEFAULT_COLUMNS)
}

/// Parses the stored `mail-list-columns` value into the configured column
/// order. Ids that no longer exist are ignored, duplicates are dropped, and an
/// empty (or entirely unknown) value falls back to [`DEFAULT_COLUMNS`], so a
/// fresh install and a corrupted setting both show Outlook's set.
pub fn parse_columns(ids: &[String]) -> Vec<MessageColumn> {
    let mut columns: Vec<MessageColumn> = Vec::new();
    for id in ids {
        if let Some(column) = MessageColumn::from_id(id) {
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
    }
    if columns.is_empty() {
        DEFAULT_COLUMNS.to_vec()
    } else {
        columns
    }
}

/// Serialises a column order for `mail-list-columns`.
pub fn columns_to_ids(columns: &[MessageColumn]) -> Vec<String> {
    columns.iter().map(|column| column.id().to_string()).collect()
}

/// The columns that fit in `available` pixels, dropping lower-priority cells
/// before the Subject is starved. From, Subject and Received always survive;
/// the rest go in this order as the pane narrows: To, Categories, Size,
/// Attachment, Flag, Importance.
///
/// Called on every list width change, but only a change in the *returned set*
/// does any work (see `window::columns`), so resizing within one band costs
/// nothing.
pub fn columns_for_width(configured: &[MessageColumn], available: i32) -> Vec<MessageColumn> {
    let mut columns = configured.to_vec();
    // Priority order of the columns to shed first.
    const SHED_ORDER: [MessageColumn; 6] = [
        MessageColumn::To,
        MessageColumn::Categories,
        MessageColumn::Size,
        MessageColumn::Attachment,
        MessageColumn::Flag,
        MessageColumn::Importance,
    ];
    // The width each candidate set actually needs - one formula, shared with
    // the automatic layout threshold (`preferred_single_line_width`).
    for column in SHED_ORDER {
        if row_width(&columns) <= available {
            break;
        }
        columns.retain(|candidate| *candidate != column);
    }
    columns
}

/// The Categories cell's text: the category names joined with ", ", or an
/// empty string when the message has none.
pub fn categories_text(names: &[&str]) -> String {
    names.join(", ")
}

/// A human-readable attachment-ish size for the Size column, matching
/// Outlook's style: whole kilobytes, one decimal for megabytes and gigabytes.
pub fn format_size(bytes: u32) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else {
        format!("{:.0} KB", (bytes / KB).max(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_ids_round_trip_and_cover_every_column() {
        for column in MessageColumn::ALL {
            assert_eq!(MessageColumn::from_id(column.id()), Some(column));
            assert!(!column.label().is_empty());
        }
        assert_eq!(MessageColumn::from_id("nope"), None);
    }

    #[test]
    fn every_column_maps_to_an_outlook_sort_key() {
        assert_eq!(MessageColumn::From.sort_key(), SortKey::Sender);
        assert_eq!(MessageColumn::Received.sort_key(), SortKey::Date);
        assert_eq!(MessageColumn::Categories.sort_key(), SortKey::Category);
        assert_eq!(MessageColumn::Attachment.sort_key(), SortKey::Attachments);
        assert_eq!(MessageColumn::Flag.sort_key(), SortKey::Flag);
        assert_eq!(MessageColumn::To.sort_key(), SortKey::To);
        assert_eq!(MessageColumn::Importance.sort_key(), SortKey::Importance);
        assert_eq!(MessageColumn::Subject.sort_key(), SortKey::Subject);
        assert_eq!(MessageColumn::Size.sort_key(), SortKey::Size);
    }

    #[test]
    fn the_default_set_is_outlooks_and_hides_to() {
        assert_eq!(
            DEFAULT_COLUMNS.to_vec(),
            MessageColumn::ALL.iter().copied().filter(|c| *c != MessageColumn::To).collect::<Vec<_>>()
        );
        assert!(MessageColumn::Subject.expands(), "only the subject takes the slack");
        assert!(MessageColumn::ALL.iter().filter(|c| c.expands()).count() == 1);
    }

    #[test]
    fn parsing_ignores_unknown_and_duplicate_ids_and_falls_back_to_defaults() {
        let parsed = parse_columns(&["received".into(), "nope".into(), "from".into(), "received".into()]);
        assert_eq!(parsed, vec![MessageColumn::Received, MessageColumn::From]);

        assert_eq!(parse_columns(&[]), DEFAULT_COLUMNS.to_vec());
        assert_eq!(parse_columns(&["nope".into()]), DEFAULT_COLUMNS.to_vec());
    }

    #[test]
    fn columns_to_ids_round_trips_through_parse() {
        let columns = vec![MessageColumn::To, MessageColumn::Subject, MessageColumn::Flag];
        assert_eq!(parse_columns(&columns_to_ids(&columns)), columns);
    }

    #[test]
    fn width_fitting_sheds_low_priority_columns_but_keeps_the_core() {
        let wide = columns_for_width(&MessageColumn::ALL, 1600);
        assert_eq!(wide, MessageColumn::ALL.to_vec());

        let narrow = columns_for_width(&MessageColumn::ALL, 600);
        assert!(narrow.contains(&MessageColumn::From));
        assert!(narrow.contains(&MessageColumn::Subject));
        assert!(narrow.contains(&MessageColumn::Received));
        assert!(!narrow.contains(&MessageColumn::To), "To is shed first");

        let tiny = columns_for_width(&MessageColumn::ALL, 200);
        assert_eq!(tiny, vec![MessageColumn::From, MessageColumn::Subject, MessageColumn::Received]);
    }

    #[test]
    fn width_fitting_is_monotone_as_the_pane_grows() {
        let mut previous: Option<Vec<MessageColumn>> = None;
        for width in (200..=1400).step_by(20) {
            let columns = columns_for_width(&MessageColumn::ALL, width);
            if let Some(previous) = previous {
                for column in &previous {
                    assert!(columns.contains(column), "{column:?} came back after being shed at {width}px");
                }
            }
            previous = Some(columns);
        }
    }

    #[test]
    fn the_automatic_threshold_is_the_default_columns_preferred_width() {
        let threshold = preferred_single_line_width();
        assert_eq!(threshold, row_width(&DEFAULT_COLUMNS));
        assert_eq!(
            columns_for_width(&DEFAULT_COLUMNS, threshold),
            DEFAULT_COLUMNS.to_vec(),
            "the default set fits exactly at the automatic threshold"
        );
        let just_narrower = columns_for_width(&DEFAULT_COLUMNS, threshold - 1);
        assert!(just_narrower.len() < DEFAULT_COLUMNS.len(), "one column is shed just below it");
    }

    #[test]
    fn size_text_matches_outlooks_units() {
        assert_eq!(format_size(0), "1 KB");
        assert_eq!(format_size(900), "1 KB");
        assert_eq!(format_size(12 * 1024), "12 KB");
        assert_eq!(format_size(1536 * 1024), "1.5 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn categories_join_with_commas_and_empty_stays_empty() {
        assert_eq!(categories_text(&["Red category", "Blue category"]), "Red category, Blue category");
        assert_eq!(categories_text(&[]), "");
    }
}
