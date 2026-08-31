//! The Lookout module: a dashboard tab snapshotting the user's connected
//! accounts - an AI Chat card (a prompt field wired by the caller to the
//! configured assistant API), people most contacted, emails by time of day,
//! outstanding tasks, and upcoming calendar events. Follows the same
//! data-in/widget-state-out convention as `tasks_view`: the caller owns
//! the data and pushes it via [`LookoutView::set_data`]; the view owns
//! only widget state. Selection and ordering logic live in pure,
//! unit-tested functions so the layout is testable independent of a
//! running GTK main loop.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use chrono::{DateTime, Duration, Local, NaiveDateTime};
use gtk::prelude::*;
use lookout_core::{CalendarId, CalendarTask, EmailAddress, EventOccurrence};
use webkit::prelude::*;

use crate::calendar_colors::{CalendarColorMap, DEFAULT_CHECK_COLOR};
use crate::tasks_view::{self, ActivateHandler, TaskBucket, ToggleHandler};

/// How many rows each dashboard section shows. `TOP_CONTACTS_LIMIT` is
/// `pub(crate)` because the caller unions each account's top list and needs
/// the same cap to re-rank by.
pub(crate) const TOP_CONTACTS_LIMIT: usize = 5;
const OUTSTANDING_TASKS_LIMIT: usize = 8;
const UPCOMING_EVENTS_LIMIT: usize = 10;
/// How far ahead the events section looks (bounded by what the sessions
/// have synced; the caller widens that by requesting the months on open).
const EVENT_HORIZON_DAYS: i64 = 14;

/// Everything the dashboard needs to repaint, merged across accounts by
/// the caller.
#[derive(Debug, Clone, Default)]
pub struct LookoutData {
    /// Most-contacted people first, with their lifetime appearance count.
    pub contacts: Vec<(EmailAddress, i64)>,
    /// Count of cached messages per local hour of day (index 0 = midnight).
    pub histogram: [i64; 24],
    /// All tasks from every source; the view keeps only the outstanding ones.
    pub tasks: Vec<CalendarTask>,
    /// Every synced occurrence; the view keeps the checked, upcoming ones.
    pub events: Vec<EventOccurrence>,
    /// Which calendars (by id) the user has checked in "My calendars".
    pub checked_calendar_ids: HashSet<CalendarId>,
    /// Per-calendar colours, for the event rows' dots.
    pub colors: CalendarColorMap,
}

/// The outstanding subset of `tasks`, in section order (Overdue, Today,
/// This week, Later) with each bucket's sort kept, truncated to `limit`.
/// Completed tasks are excluded - the dashboard is a to-do, not an archive.
pub fn outstanding_tasks(tasks: &[CalendarTask], now: NaiveDateTime, limit: usize) -> Vec<CalendarTask> {
    let mut out = Vec::new();
    for (bucket, bucket_tasks) in tasks_view::group_tasks(tasks, now) {
        if bucket == TaskBucket::Completed {
            continue;
        }
        out.extend(bucket_tasks);
        if out.len() >= limit {
            break;
        }
    }
    out.truncate(limit);
    out
}

/// The `limit` next occurrences at or after `now` (and within `horizon`),
/// restricted to the checked calendars, sorted by start time.
pub fn upcoming_occurrences<'a>(
    occurrences: impl IntoIterator<Item = &'a EventOccurrence>,
    now: DateTime<Local>,
    horizon: Duration,
    checked: &HashSet<CalendarId>,
    limit: usize,
) -> Vec<&'a EventOccurrence> {
    let horizon_end = now + horizon;
    let mut upcoming: Vec<&'a EventOccurrence> = occurrences
        .into_iter()
        .filter(|occ| checked.contains(&occ.calendar_id))
        .filter(|occ| {
            let start = occ.start.with_timezone(&Local);
            start >= now && start <= horizon_end
        })
        .collect();
    upcoming.sort_by_key(|occ| occ.start);
    upcoming.truncate(limit);
    upcoming
}

/// The Lookout view's widget state. `root` is the scrolled dashboard;
/// callers read nothing back from it - `set_data` repaints from scratch on
/// every push. The task rows' callbacks are stored on the view (via
/// [`LookoutView::set_handlers`]) so `set_data` callers don't need to know
/// about the window or the session routing.
pub struct LookoutView {
    pub root: gtk::ScrolledWindow,
    /// The AI Chat card's prompt field, Go button, and reply view. The
    /// caller wires the button (and the entry's Enter key) to the assistant
    /// API and loads each reply into `chat_output` as HTML (the output of
    /// [`chat_reply_html`], wrapped in the reading pane's CSP document by
    /// the caller, so a reply can carry formatting and graphics but never
    /// scripts or navigation); the dashboard's data repaints never touch
    /// these widgets, so an in-flight conversation survives every refresh.
    pub chat_entry: gtk::Entry,
    pub chat_button: gtk::Button,
    pub chat_output: webkit::WebView,
    contacts_list: gtk::Box,
    histogram_area: gtk::DrawingArea,
    histogram: Rc<RefCell<[i64; 24]>>,
    histogram_caption: gtk::Label,
    tasks_list: gtk::Box,
    events_list: gtk::Box,
    toggle: RefCell<Option<ToggleHandler>>,
    activate: RefCell<Option<ActivateHandler>>,
}

/// Builds a fresh Lookout dashboard: a scrollable two-column grid - left,
/// the AI Chat card; right, the hour histogram over people-most-contacted
/// over outstanding-tasks over upcoming events - each card with an
/// empty-state placeholder.
pub fn build_lookout_view() -> LookoutView {
    install_lookout_css();

    // --- AI Chat: a prompt field (placeholder "Summarize my inbox") with
    // a green Go button, over the assistant's reply rendered in a read-only
    // WebKit view - so the agent can answer with formatted text, tables,
    // and embedded graphics (markdown images or inline SVG in a ```html
    // fence) instead of a plain label. The caller loads each reply through
    // the same CSP wrapper as the reading pane, keeping scripts, frames,
    // and navigation off whatever HTML arrives. The card (and the reply
    // view inside it) expands vertically so the AI Chat card fills the
    // whole height of its column.
    let (chat_card, chat_box) = section_card("Ask");
    chat_card.set_vexpand(true);
    chat_box.set_vexpand(true);
    let chat_entry = gtk::Entry::builder().placeholder_text("Summarize my inbox").hexpand(true).build();
    let chat_button = gtk::Button::with_label("Ask");
    chat_button.add_css_class("suggested-action");
    chat_button.set_tooltip_text(Some("Ask the configured assistant"));
    let chat_input_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    chat_input_row.append(&chat_entry);
    chat_input_row.append(&chat_button);
    chat_box.append(&chat_input_row);
    let chat_settings = webkit::Settings::new();
    chat_settings.set_enable_javascript(false);
    chat_settings.set_enable_developer_extras(false);
    chat_settings.set_hardware_acceleration_policy(webkit::HardwareAccelerationPolicy::Never);
    let chat_output = webkit::WebView::builder()
        .settings(&chat_settings)
        .editable(false)
        .height_request(220)
        .vexpand(true)
        .build();
    // The card sits on the dashboard's dark scrim; a transparent view
    // background (plus the document's own `background: transparent` rule,
    // see `chat_document`) keeps WebKit from painting its default white.
    chat_output.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    chat_output.load_html(&chat_empty_document(), None);
    chat_box.append(&chat_output);

    let (histogram_card, histogram_box) = section_card("Emails by time of day");
    // The histogram card sits above the contacts card, which is usually
    // taller; expand the box and the chart area so the painted chart fills
    // the card instead of leaving a dead band below the caption.
    histogram_box.set_vexpand(true);

    let histogram = Rc::new(RefCell::new([0i64; 24]));
    let histogram_for_draw = histogram.clone();
    let histogram_area = gtk::DrawingArea::builder().height_request(120).hexpand(true).vexpand(true).build();
    histogram_area.set_draw_func(move |_, cr, width, height| {
        draw_histogram(cr, width as f64, height as f64, &histogram_for_draw.borrow());
    });
    histogram_box.append(&histogram_area);
    let histogram_caption = gtk::Label::builder().css_classes(["dim-label", "caption"]).xalign(0.0).build();
    histogram_box.append(&histogram_caption);

    let (contacts_card, contacts_list) = section_card("People most contacted");
    let (tasks_card, tasks_list) = section_card("Outstanding tasks");
    let (events_card, events_list) = section_card("Upcoming events");

    let column_left = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).vexpand(true).build();
    column_left.append(&chat_card);
    let column_right = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).vexpand(true).build();
    column_right.append(&histogram_card);
    column_right.append(&contacts_card);
    column_right.append(&tasks_card);
    column_right.append(&events_card);

    let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    content.append(&column_left);
    content.append(&column_right);
    let root = gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    LookoutView {
        root,
        chat_entry,
        chat_button,
        chat_output,
        contacts_list,
        histogram_area,
        histogram,
        histogram_caption,
        tasks_list,
        events_list,
        toggle: RefCell::new(None),
        activate: RefCell::new(None),
    }
}

/// A dashboard card: the `card_section` look (rounded `.card`, margin)
/// with a heading label and a repaint-from-scratch content box. The heading
/// and rows get real margins from the card's border (the app's other
/// modules put 8-12px between a `.card`'s edge and its content).
fn section_card(title: &str) -> (gtk::Box, gtk::Box) {
    let heading = gtk::Label::builder()
        .label(title)
        .css_classes(["heading"])
        .xalign(0.0)
        .margin_top(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(10)
        .build();
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"])
        .overflow(gtk::Overflow::Hidden)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .hexpand(true)
        .build();
    card.append(&heading);
    card.append(&content);
    (card, content)
}

/// Stores the task rows' callbacks (completion toggle, click-to-edit).
/// Called once at startup; every later `set_data` repaint reuses them.
pub fn set_handlers(view: &LookoutView, toggle: ToggleHandler, activate: ActivateHandler) {
    *view.toggle.borrow_mut() = Some(toggle);
    *view.activate.borrow_mut() = Some(activate);
}

/// Repaints the whole dashboard from `data`. Rebuilds rows from scratch on
/// every call - the same repaint-from-scratch convention as the calendar
/// checklist and the Tasks view.
pub fn set_data(view: &LookoutView, data: &LookoutData) {
    let toggle = view.toggle.borrow().clone().unwrap_or_else(|| Rc::new(|_t, _c| {}));
    let activate = view.activate.borrow().clone().unwrap_or_else(|| Rc::new(|_t| {}));
    let now_local = Local::now();

    // --- People most contacted.
    clear_box(&view.contacts_list);
    if data.contacts.is_empty() {
        append_empty(&view.contacts_list, "No mail history yet - your most-contacted people appear here.");
    } else {
        for (address, count) in data.contacts.iter().take(TOP_CONTACTS_LIMIT) {
            view.contacts_list.append(&contact_row(address, *count));
        }
    }

    // --- Emails by time of day.
    *view.histogram.borrow_mut() = data.histogram;
    let total: i64 = data.histogram.iter().sum();
    view.histogram_caption
        .set_label(&format!("{total} cached message{} · all time", if total == 1 { "" } else { "s" }));
    view.histogram_area.queue_draw();

    // --- Outstanding tasks.
    clear_box(&view.tasks_list);
    let outstanding = outstanding_tasks(&data.tasks, now_local.naive_local(), OUTSTANDING_TASKS_LIMIT);
    if outstanding.is_empty() {
        append_empty(&view.tasks_list, "No outstanding tasks.");
    } else {
        for task in outstanding {
            view.tasks_list
                .append(&tasks_view::task_row(&task, &data.colors, toggle.clone(), activate.clone(), &[], true));
        }
    }

    // --- Upcoming events.
    clear_box(&view.events_list);
    let upcoming = upcoming_occurrences(
        data.events.iter(),
        now_local,
        Duration::days(EVENT_HORIZON_DAYS),
        &data.checked_calendar_ids,
        UPCOMING_EVENTS_LIMIT,
    );
    if upcoming.is_empty() {
        append_empty(&view.events_list, "No upcoming events.");
    } else {
        for occ in upcoming {
            view.events_list.append(&event_row(occ, &data.colors, now_local));
        }
    }
}

/// One contact row: a display name (falling back to the address), the
/// address as a dim subtitle, and a right-aligned lifetime count.
fn contact_row(address: &EmailAddress, count: i64) -> gtk::Box {
    let name = gtk::Label::builder()
        .label(address.name.clone().unwrap_or_else(|| address.address.clone()))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_start(4)
        .margin_end(4)
        .margin_top(2)
        .margin_bottom(2)
        .build();
    row_box.append(&name);
    if address.name.is_some() {
        let address_label = gtk::Label::builder()
            .label(&address.address)
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        row_box.append(&address_label);
    }
    let count_label = gtk::Label::builder().label(count.to_string()).css_classes(["lookout-count"]).build();
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(&row_box);
    row.append(&count_label);
    row
}

/// One upcoming-event row: a calendar colour dot, the summary, and a
/// right-aligned time caption ("All day", today's time, or day + time).
fn event_row(occ: &EventOccurrence, colors: &CalendarColorMap, now: DateTime<Local>) -> gtk::Box {
    let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or(DEFAULT_CHECK_COLOR).to_string();
    let dot = gtk::DrawingArea::builder().width_request(10).height_request(10).valign(gtk::Align::Center).build();
    dot.set_draw_func(move |_, cr, width, height| {
        let (r, g, b) = tasks_view::parse_css_color(&color);
        let radius = width.min(height) as f64 / 2.0 - 1.0;
        cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius, 0.0, 2.0 * std::f64::consts::PI);
        cr.set_source_rgba(r, g, b, 1.0);
        let _ = cr.fill();
    });
    dot.set_tooltip_text(Some(&occ.calendar_id.0));

    let summary = gtk::Label::builder()
        .label(occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string()))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let start = occ.start.with_timezone(&Local);
    let time_text = if occ.all_day {
        "All day".to_string()
    } else if start.date_naive() == now.date_naive() {
        start.format("%H:%M").to_string()
    } else {
        start.format("%a %d %b · %H:%M").to_string()
    };
    let time = gtk::Label::builder().label(&time_text).css_classes(["dim-label", "caption"]).build();

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(4)
        .margin_end(4)
        .build();
    row_box.append(&dot);
    row_box.append(&summary);
    row_box.append(&time);
    row_box
}

/// Paints the 24-bar hour histogram: bars scaled to the largest hour, with
/// hour tick labels at 0/6/12/18/23. A "No mail yet" placeholder replaces
/// the chart while the total is zero.
fn draw_histogram(cr: &gtk::cairo::Context, width: f64, height: f64, histogram: &[i64; 24]) {
    let pad_top = 8.0;
    let pad_bottom = 18.0;
    let pad_x = 8.0;
    let total: i64 = histogram.iter().sum();
    let max: i64 = histogram.iter().copied().max().unwrap_or(0);

    let chart_height = height - pad_top - pad_bottom;
    let plot_width = width - 2.0 * pad_x;

    if total == 0 || max == 0 || chart_height <= 0.0 || plot_width <= 0.0 {
        cr.select_font_face("sans-serif", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
        cr.set_font_size(10.0);
        let extents = cr.text_extents("No mail yet").ok();
        let (advance, text_height, y_bearing) = match &extents {
            Some(e) => (e.x_advance(), e.height(), e.y_bearing()),
            None => (0.0, 0.0, 0.0),
        };
        cr.move_to((width - advance) / 2.0, (height - text_height) / 2.0 - y_bearing);
        cr.set_source_rgba(0.5, 0.5, 0.5, 1.0);
        let _ = cr.show_text("No mail yet");
        return;
    }

    let gap = 2.0;
    let bar_width = (plot_width - gap * 23.0) / 24.0;
    if bar_width <= 0.0 {
        return;
    }

    // Baseline.
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.4);
    cr.set_line_width(1.0);
    cr.move_to(pad_x, height - pad_bottom);
    cr.line_to(pad_x + plot_width, height - pad_bottom);
    let _ = cr.stroke();

    // Bars.
    cr.set_source_rgba(0.384, 0.627, 0.918, 1.0); // #62a0ea
    for (hour, count) in histogram.iter().enumerate() {
        let x = pad_x + hour as f64 * (bar_width + gap);
        let bar_height = (*count as f64 / max as f64) * chart_height;
        cr.rectangle(x, height - pad_bottom - bar_height, bar_width, bar_height);
        let _ = cr.fill();
    }

    // Hour tick labels.
    cr.select_font_face("sans-serif", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    cr.set_font_size(9.0);
    for &hour in &[0, 6, 12, 18, 23] {
        let text = hour.to_string();
        let advance = cr.text_extents(&text).ok().map(|e| e.x_advance()).unwrap_or(0.0);
        let x = pad_x + hour as f64 * (bar_width + gap) + (bar_width - advance) / 2.0;
        cr.move_to(x.max(0.0), height - pad_bottom + 12.0);
        let _ = cr.show_text(&text);
    }
}

fn clear_box(box_: &gtk::Box) {
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
}

fn append_empty(box_: &gtk::Box, text: &str) {
    let placeholder = gtk::Label::builder()
        .label(text)
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .wrap(true)
        .margin_top(8)
        .build();
    box_.append(&placeholder);
}

/// The dashboard's handful of display-level CSS rules, installed once per
/// process (multiple installs are harmless, but a `Once` keeps it to one).
fn install_lookout_css() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".lookout-count {
                color: @lookout-unread;
                font-weight: bold;
                font-feature-settings: 'tnum';
            }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
}

/// One block-level unit [`parse_blocks`] recognizes: either a heading
/// (level 1-4, already inline-formatted) that [`chat_reply_cards_html`] may
/// split a card on, or a fully-rendered other block (paragraph, list,
/// fenced code, ```html passthrough) that always stays wherever it's put.
enum Block {
    Heading { level: usize, title_html: String },
    Html(String),
}

/// A [`chat_reply_cards_html`] card: an optional heading (its level and
/// already-rendered title) plus the rendered HTML of everything under it.
struct Section {
    title: Option<(usize, String)>,
    body: String,
}

/// Flushes `paragraph` (via [`flush_paragraph`]) and, if that produced any
/// HTML, appends it to `blocks` as a [`Block::Html`].
fn flush_paragraph_block(paragraph: &mut String, blocks: &mut Vec<Block>) {
    let mut html = String::new();
    flush_paragraph(paragraph, &mut html);
    if !html.is_empty() {
        blocks.push(Block::Html(html));
    }
}

/// The markdown block parser behind [`chat_reply_cards_html`]: a tiny
/// markdown-to-HTML renderer over fully escaped text, so a reply can never
/// smuggle in markup the caller didn't ask for - the caller wraps the
/// result in the reading pane's CSP document before loading it into the
/// chat WebView, so this function is deliberately *not* the security
/// boundary, only the formatting pass.
///
/// Supported: `#`-`####` headings, `**bold**`, `*italic*`, `` `code` ``
/// and fenced ``` fences, `[text](url)` links, `![alt](url)` images,
/// `-`/`*` bullet and `1.` numbered lists, and blank-line paragraphs with
/// single newlines as line breaks. A fenced block tagged ```html is the
/// one passthrough: its contents are inserted verbatim (still under the
/// caller's CSP), which is how the agent embeds graphics - inline SVG,
/// `<table>`s, `<img>` - that markdown can't express.
fn parse_blocks(reply: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut lines = reply.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            flush_paragraph_block(&mut paragraph, &mut blocks);
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            flush_paragraph_block(&mut paragraph, &mut blocks);
            let fence_lang = trimmed.trim_start_matches('`').trim();
            let mut code: Vec<&str> = Vec::new();
            for code_line in lines.by_ref() {
                if code_line.trim_start().starts_with("```") {
                    break;
                }
                code.push(code_line);
            }
            let code_text = code.join("\n");
            if fence_lang.eq_ignore_ascii_case("html") {
                // The graphics passthrough: verbatim HTML, CSP-guarded by
                // the caller when it loads the document.
                blocks.push(Block::Html(format!("{code_text}\n")));
            } else {
                blocks.push(Block::Html(format!("<pre><code>{}</code></pre>\n", escape_html(&code_text))));
            }
            continue;
        }
        if let Some((level, content)) = heading(trimmed) {
            flush_paragraph_block(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading {
                level,
                title_html: inline_html(content),
            });
            continue;
        }
        if let Some((ordered, content)) = list_item(trimmed) {
            flush_paragraph_block(&mut paragraph, &mut blocks);
            let mut items: Vec<String> = vec![inline_html(content)];
            while let Some(next) = lines.peek().map(|l| l.trim_start()) {
                let Some((same_kind, item_content)) = list_item(next) else { break };
                if same_kind != ordered {
                    break;
                }
                items.push(inline_html(item_content));
                lines.next();
            }
            let tag = if ordered { "ol" } else { "ul" };
            let mut html = format!("<{tag}>\n");
            for item in &items {
                html.push_str(&format!("<li>{item}</li>\n"));
            }
            html.push_str(&format!("</{tag}>\n"));
            blocks.push(Block::Html(html));
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push('\n');
        }
        paragraph.push_str(&inline_html(trimmed));
    }
    flush_paragraph_block(&mut paragraph, &mut blocks);
    blocks
}

/// The assistant's reply grouped into visual "cards" by top-level heading:
/// each `#`/`##` heading starts a new card (its own tag is suppressed from
/// the body and reused as the card's title, styled by the `.lookout-card-
/// title` rule in [`chat_document`]); `###`/`####` headings stay inline
/// inside whichever card they fall in - they mark sub-structure within a
/// topic, not a new one. A reply is always at least one card: a reply with
/// no `#`/`##` headings is a single untitled card holding the whole reply,
/// and any content before the first `#`/`##` heading becomes its own
/// leading untitled card rather than being left card-less or folded into
/// the first titled section.
pub fn chat_reply_cards_html(reply: &str) -> String {
    let mut sections = vec![Section { title: None, body: String::new() }];
    for block in parse_blocks(reply) {
        match block {
            Block::Heading { level, title_html } if level <= 2 => {
                let claim_first = sections.len() == 1 && sections[0].title.is_none() && sections[0].body.is_empty();
                if claim_first {
                    sections[0].title = Some((level, title_html));
                } else {
                    sections.push(Section {
                        title: Some((level, title_html)),
                        body: String::new(),
                    });
                }
            }
            Block::Heading { level, title_html } => {
                sections
                    .last_mut()
                    .expect("always at least one section")
                    .body
                    .push_str(&format!("<h{level}>{title_html}</h{level}>\n"));
            }
            Block::Html(html) => {
                sections.last_mut().expect("always at least one section").body.push_str(&html);
            }
        }
    }
    if sections.len() == 1 && sections[0].title.is_none() && sections[0].body.is_empty() {
        return String::new();
    }
    sections.into_iter().map(render_card).collect()
}

/// Renders one [`chat_reply_cards_html`] section as a `.lookout-card` div:
/// its title (if any) as an `.lookout-card-title` heading, then its body.
fn render_card(section: Section) -> String {
    let mut out = String::from("<div class=\"lookout-card\">");
    if let Some((level, title_html)) = section.title {
        out.push_str(&format!("<h{level} class=\"lookout-card-title\">{title_html}</h{level}>"));
    }
    out.push_str(&section.body);
    out.push_str("</div>");
    out
}

/// The full document the caller loads into the chat WebView: a rendered
/// reply (`chat_reply_cards_html` output or a plain status string) inside
/// the reading pane's CSP wrapper at the images-allowed level - remote
/// images may load, everything else remote stays blocked - with an
/// explicit `background: transparent` rule so the card shows the
/// dashboard's dark scrim behind the reply instead of WebKit's default
/// white, and a grey default text colour (`#e5e5e5`, the 90% grey of the
/// CSS/SVG grey ramp) so uncoloured reply text is readable on that scrim
/// without being stark white. Links get an explicit `a`/`a:visited` colour
/// (`#4d9dff`, the same accent the app already uses for unread badges in
/// its dark theme, see `flat-dark.css`'s `@lookout-unread`) rather than the
/// WebKit UA stylesheet's default link blue, which reads low-contrast on
/// this dark scrim; everything the agent styles itself still wins.
///
/// Also carries the `.lookout-card`/`.lookout-card-title` rules
/// `chat_reply_cards_html` wraps each section in: WebKit's CSS engine can't
/// see libadwaita's `.card` style or its theme variables, so the look is
/// approximated with literal translucent-white-on-dark values (in the same
/// spirit as this function's own literal `#e5e5e5`) rather than reused from
/// the native dashboard cards' `section_card` GTK styling. These rules are
/// simply unused (harmless dead CSS) in the empty/loading/error documents,
/// which never emit a `.lookout-card` element.
pub fn chat_document(html: &str) -> String {
    let styled = format!(
        "<style>html, body {{ background: transparent; color: #e5e5e5; }}\
         a, a:visited {{ color: #4d9dff; }}\
         .lookout-card {{ background: rgba(255, 255, 255, 0.07); border: 1px solid rgba(255, 255, 255, 0.09); \
         border-radius: 12px; margin: 6px; padding: 12px; box-sizing: border-box; }}\
         .lookout-card-title {{ margin: 0 0 6px 0; font-size: 1.05em; font-weight: bold; color: #e5e5e5; }}\
         .lookout-card > *:first-child {{ margin-top: 0; }}\
         .lookout-card > *:last-child {{ margin-bottom: 0; }}</style>{html}"
    );
    crate::window::wrap_message_with_csp(&styled, true, false)
}

/// The empty-state document shown in the chat view before the first
/// question is asked: no placeholder text, just the assistant robot glyph
/// centred in the view. The artwork is the bundled `ai-1.svg` (see
/// [`chat_empty_svg`]); its strokes are drawn black (the SVG's own
/// `stroke-opacity="0.9"`), so the document lightens it with an `invert(1)`
/// filter plus a 60% opacity - the app's `flat-dark` dashboard is a dark
/// pane, and black-on-dark would be invisible (the same reasoning that drew
/// the empty-folder bird white). The first real load ("Asking the
/// assistant…", then the reply) replaces this document.
pub fn chat_empty_document() -> String {
    let svg = chat_empty_svg();
    let fragment = format!(
        "<style>\
         body {{ margin: 0; display: flex; align-items: center; justify-content: center; min-height: 100%; }}\
         .chat-empty-icon {{ width: 88px; height: 88px; }}\
         .chat-empty-icon svg {{ width: 88px; height: 88px; opacity: 0.6; filter: invert(1); }}\
         </style>\
         <div class=\"chat-empty-icon\">{svg}</div>"
    );
    chat_document(&fragment)
}

/// The document shown in the chat view while a question is in flight: the
/// same robot glyph as the idle state (`chat_empty_document`), lightened
/// the same `invert(1)` way, but pulsing - a soft opacity fade in and out -
/// instead of sitting static at a fixed 60%, so there's a visible sign of
/// life for however long the configured assistant takes to answer. No
/// placeholder text, matching the idle state's icon-only look; the reply
/// (or [`chat_error_document`] on failure) replaces this document.
pub fn chat_loading_document() -> String {
    let svg = chat_empty_svg();
    let fragment = format!(
        "<style>\
         body {{ margin: 0; display: flex; align-items: center; justify-content: center; min-height: 100%; }}\
         .chat-loading-icon {{ width: 88px; height: 88px; }}\
         .chat-loading-icon svg {{ width: 88px; height: 88px; filter: invert(1); animation: chat-icon-pulse 1.4s ease-in-out infinite; }}\
         @keyframes chat-icon-pulse {{ 0%, 100% {{ opacity: 0.2; }} 50% {{ opacity: 0.6; }} }}\
         </style>\
         <div class=\"chat-loading-icon\">{svg}</div>"
    );
    chat_document(&fragment)
}

/// The document shown when a question fails: the robot glyph recolored to
/// the app's standard error red (`#e01b24` - the same GNOME red used
/// elsewhere for tags, calendar colors, and the tray badge), with the error
/// text underneath so the failure stays diagnosable (a bad URL, an expired
/// token, the model rejecting the request, ...) instead of just a mute red
/// icon.
///
/// Recoloring goes through a CSS mask rather than the idle/loading states'
/// `filter: invert(1)`: a `filter` can only rotate/invert the artwork's
/// *existing* colors, which gets you *a* different color but not
/// necessarily *this exact* one, whereas masking a solid `background-color`
/// through the artwork's alpha channel (via [`chat_icon_data_uri`]) matches
/// the target hex exactly regardless of the SVG's own stroke colors.
pub fn chat_error_document(message: &str) -> String {
    let icon_uri = chat_icon_data_uri();
    let fragment = format!(
        "<style>\
         body {{ margin: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; \
         min-height: 100%; gap: 12px; text-align: center; padding: 12px; box-sizing: border-box; }}\
         .chat-error-icon {{ width: 88px; height: 88px; flex-shrink: 0; background-color: #e01b24; \
         -webkit-mask-image: url(\"{icon_uri}\"); -webkit-mask-repeat: no-repeat; -webkit-mask-position: center; -webkit-mask-size: contain; \
         mask-image: url(\"{icon_uri}\"); mask-repeat: no-repeat; mask-position: center; mask-size: contain; }}\
         .chat-error-message {{ margin: 0; font-size: 0.95em; }}\
         </style>\
         <div class=\"chat-error-icon\"></div>\
         <p class=\"chat-error-message\">{}</p>",
        escape_html(message)
    );
    chat_document(&fragment)
}

/// The robot glyph artwork's raw bytes, through the app's asset pipeline:
/// the compiled GResource's `ai-1.svg` when the bundle is registered (the
/// normal runtime path, see `resources::register`), otherwise the
/// compile-time `include_bytes!` copy - the same convention as
/// `window::svg_image`'s fallback for builds whose bundle couldn't be
/// compiled.
fn chat_icon_bytes() -> Vec<u8> {
    crate::resources::bytes("/io/github/gavindi/Lookout/icons/ai-1.svg")
        .map(|bytes| bytes.to_vec())
        .unwrap_or_else(|| include_bytes!("../../../data/resources/icons/ai-1.svg").to_vec())
}

/// [`chat_icon_bytes`] with its leading XML declaration stripped, so it
/// embeds cleanly as an inline element of the HTML document.
fn chat_empty_svg() -> String {
    let bytes = chat_icon_bytes();
    let source = String::from_utf8_lossy(&bytes);
    let trimmed = source.trim_start();
    let body = trimmed
        .strip_prefix("<?xml")
        .and_then(|rest| rest.find("?>").map(|end| &rest[end + 2..]))
        .unwrap_or(trimmed);
    body.trim_start().to_string()
}

/// [`chat_icon_bytes`] as a `data:image/svg+xml;base64,...` URI, for the
/// CSS mask [`chat_error_document`] recolors the artwork through.
fn chat_icon_data_uri() -> String {
    use base64::Engine;
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(chat_icon_bytes()))
}

/// Flushes a completed paragraph: `<p>` around its inline HTML, with each
/// source newline a `<br>` (a blank line ends a paragraph, a single
/// newline breaks the line).
fn flush_paragraph(paragraph: &mut String, out: &mut String) {
    if paragraph.is_empty() {
        return;
    }
    out.push_str("<p>");
    for (index, line) in paragraph.split('\n').enumerate() {
        if index > 0 {
            out.push_str("<br>");
        }
        out.push_str(line);
    }
    out.push_str("</p>\n");
    paragraph.clear();
}

/// `#`-`####` heading lines: `Some((level, content))` when the line is one,
/// with the marker stripped.
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if (1..=4).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some((hashes, &line[hashes + 1..]))
    } else {
        None
    }
}

/// A list-item line: `Some((ordered, content))` for `- ` / `* ` bullets
/// (ordered = false) and any `N. `-style numbered item (ordered = true).
/// The rendered `<ol>` re-numbers, so the source's specific digits don't
/// need preserving.
fn list_item(line: &str) -> Option<(bool, &str)> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some((false, rest));
    }
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return Some((true, &line[digits + 2..]));
    }
    None
}

/// One line's inline formatting: code spans first (their content is
/// escaped verbatim, so markup inside them stays literal), then images and
/// links, then `**bold**` / `*italic*`. Everything else is escaped, so the
/// output's only tags are the ones this function writes.
fn inline_html(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        if let Some(after_backtick) = rest.strip_prefix('`') {
            let Some(end) = after_backtick.find('`') else {
                out.push_str(&escape_html(rest));
                break;
            };
            out.push_str("<code>");
            out.push_str(&escape_html(&after_backtick[..end]));
            out.push_str("</code>");
            rest = &after_backtick[end + 1..];
            continue;
        }
        if let Some(after_bang) = rest.strip_prefix("![") {
            if let Some((alt, (url, after))) = bracket_paren(after_bang) {
                out.push_str(&format!("<img src=\"{}\" alt=\"{}\">", escape_attribute(url), escape_attribute(alt)));
                rest = after;
                continue;
            }
        }
        if let Some(after_bracket) = rest.strip_prefix('[') {
            if let Some((text, (url, after))) = bracket_paren(after_bracket) {
                out.push_str(&format!("<a href=\"{}\">{}</a>", escape_attribute(url), inline_html(text)));
                rest = after;
                continue;
            }
        }
        if let Some(after_stars) = rest.strip_prefix("**") {
            if let Some(end) = after_stars.find("**") {
                out.push_str("<strong>");
                out.push_str(&inline_html(&after_stars[..end]));
                out.push_str("</strong>");
                rest = &after_stars[end + 2..];
                continue;
            }
        }
        if let Some(after_star) = rest.strip_prefix('*') {
            if let Some(end) = after_star.find('*') {
                out.push_str("<em>");
                out.push_str(&escape_html(&after_star[..end]));
                out.push_str("</em>");
                rest = &after_star[end + 1..];
                continue;
            }
        }
        let ch = rest.chars().next().expect("rest is non-empty");
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Parses the `text](url)` half of a `[text](url)` / `![alt](url)`
/// construct, given the text already past the opening `[`: the text up to
/// the closing `]`, then the URL between the following `(` and `)`.
///
/// Also accepts CommonMark's angle-bracket destination form, `(<url>)` -
/// the model's own tool-provided deep links (`chat_links`) are long,
/// punctuation-heavy strings, and some models wrap exactly that kind of URL
/// in `< >` rather than leaving it bare. Without this, the angle brackets
/// would be parsed as part of the URL itself instead of its delimiters,
/// silently breaking the link.
fn bracket_paren(s: &str) -> Option<(&str, (&str, &str))> {
    let close = s.find(']')?;
    let after = s[close + 1..].strip_prefix('(')?;
    if let Some(inside) = after.strip_prefix('<') {
        let end = inside.find('>')?;
        let rest = inside[end + 1..].strip_prefix(')')?;
        return Some((&s[..close], (&inside[..end], rest)));
    }
    let end = after.find(')')?;
    Some((&s[..close], (&after[..end], &after[end + 1..])))
}

/// Escapes text for the document body: `&`, `<`, `>` only, so the renderer
/// keeps its own tags while everything the reply said stays literal.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escapes text for a quoted attribute value: `escape_html` plus quotes.
fn escape_attribute(s: &str) -> String {
    escape_html(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{TaskStatus, TaskUid};

    fn cal(id: &str) -> CalendarId {
        CalendarId(id.to_string())
    }

    fn task(uid: &str, due_day: u32, status: TaskStatus) -> CalendarTask {
        CalendarTask {
            uid: TaskUid(uid.to_string()),
            calendar_id: cal("test:cal"),
            summary: Some(uid.to_string()),
            description: None,
            due: Some(Local.with_ymd_and_hms(2026, 8, due_day, 9, 0, 0).unwrap().with_timezone(&Utc)),
            start: None,
            completed: None,
            status,
            priority: Default::default(),
            percent_complete: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    fn occ(uid: &str, day: u32, hour: u32) -> EventOccurrence {
        EventOccurrence {
            uid: lookout_core::EventUid(uid.to_string()),
            calendar_id: cal("test:cal"),
            summary: Some(uid.to_string()),
            description: None,
            location: None,
            start: Local.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap().with_timezone(&Utc),
            end: Local.with_ymd_and_hms(2026, 8, day, hour + 1, 0, 0).unwrap().with_timezone(&Utc),
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
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            transparency: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn outstanding_tasks_excludes_completed_and_keeps_bucket_order() {
        let now = NaiveDateTime::parse_from_str("2026-08-10 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let tasks = vec![
            task("later", 20, TaskStatus::NeedsAction),
            task("completed", 1, TaskStatus::Completed),
            task("overdue", 8, TaskStatus::NeedsAction),
            task("today", 10, TaskStatus::NeedsAction),
        ];
        let outstanding = outstanding_tasks(&tasks, now, 10);
        let order: Vec<&str> = outstanding.iter().map(|t| t.summary.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["overdue", "today", "later"], "bucket order with completed dropped");

        let truncated = outstanding_tasks(&tasks, now, 2);
        assert_eq!(truncated.len(), 2);
        assert_eq!(truncated[0].summary.as_deref(), Some("overdue"));
        assert_eq!(truncated[1].summary.as_deref(), Some("today"));
    }

    #[test]
    fn upcoming_occurrences_filters_checks_and_horizon_and_sorts() {
        let now = Local.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let checked: HashSet<CalendarId> = [cal("test:cal")].into_iter().collect();
        let occurrences = [
            occ("hidden", 10, 15), // unchecked calendar
            occ("later", 12, 9),   // within horizon
            occ("past", 9, 11),    // before now
            occ("far", 30, 9),     // beyond the 14-day horizon
            occ("soon", 10, 14),   // next, sorted first
        ];
        let mut hidden = occurrences[0].clone();
        hidden.calendar_id = cal("other:cal");

        let upcoming = upcoming_occurrences(
            [&occurrences[1], &hidden, &occurrences[2], &occurrences[3], &occurrences[4]],
            now,
            Duration::days(14),
            &checked,
            10,
        );
        let order: Vec<&str> = upcoming.iter().map(|o| o.summary.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["soon", "later"]);

        let limited = upcoming_occurrences([&occurrences[1], &hidden, &occurrences[4]], now, Duration::days(14), &checked, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].summary.as_deref(), Some("soon"));
    }

    #[test]
    fn chat_reply_cards_html_escapes_plain_text_and_builds_paragraphs_in_an_untitled_card() {
        // Everything the reply says is escaped, so a reply can never smuggle
        // in its own tags.
        let cards = chat_reply_cards_html("Hello <b>world</b> & friends\nsecond line\n\nNew paragraph.");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><p>Hello &lt;b&gt;world&lt;/b&gt; &amp; friends<br>second line</p>\n<p>New paragraph.</p>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_renders_bold_italic_links_and_code_in_a_titled_card() {
        let cards = chat_reply_cards_html("# Heading\n\n**bold** and *italic* and `code` and [a link](https://example.org).");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><h1 class=\"lookout-card-title\">Heading</h1><p><strong>bold</strong> and <em>italic</em> and <code>code</code> and <a href=\"https://example.org\">a link</a>.</p>\n</div>"
        );
    }

    /// CommonMark's angle-bracket destination form, `[text](<url>)`. A model
    /// asked to link to a `chat_links`-built deep link (long, punctuation-
    /// heavy) sometimes wraps it this way; without special-casing it the
    /// angle brackets would be parsed as part of the URL itself rather than
    /// its delimiters, so the emitted `href` would never match a real
    /// scheme and the link would silently do nothing when clicked.
    #[test]
    fn chat_reply_cards_html_strips_angle_brackets_around_a_link_destination() {
        let cards = chat_reply_cards_html("[Team sync](<lookout-action:open-event?data=a%3Ab>)");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><p><a href=\"lookout-action:open-event?data=a%3Ab\">Team sync</a></p>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_escapes_fenced_code_but_passes_html_fences_verbatim_in_an_untitled_card() {
        let code = chat_reply_cards_html("```\nlet x = 1 < 2;\n```");
        assert_eq!(code, "<div class=\"lookout-card\"><pre><code>let x = 1 &lt; 2;</code></pre>\n</div>");

        // The ```html fence is the graphics passthrough: verbatim, so the
        // agent can embed SVG/tables/imgs that markdown can't express (the
        // caller's CSP wrapper is the actual safety boundary).
        let graphic = chat_reply_cards_html("```html\n<svg width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>\n```");
        assert_eq!(
            graphic,
            "<div class=\"lookout-card\"><svg width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_renders_bullet_and_numbered_lists_in_an_untitled_card() {
        let cards = chat_reply_cards_html("- one\n- two\n\n1. first\n2. second");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><ul>\n<li>one</li>\n<li>two</li>\n</ul>\n<ol>\n<li>first</li>\n<li>second</li>\n</ol>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_renders_images_and_escapes_their_attributes() {
        let cards = chat_reply_cards_html("![a \"quote\" & chart](https://example.org/chart?x=1&y=2)");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><p><img src=\"https://example.org/chart?x=1&amp;y=2\" alt=\"a &quot;quote&quot; &amp; chart\"></p>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_splits_on_h1_and_h2_but_keeps_h3_h4_inline() {
        let cards = chat_reply_cards_html("# Topic A\nfirst\n\n## Topic B\nsecond\n\n### Detail\nthird");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><h1 class=\"lookout-card-title\">Topic A</h1><p>first</p>\n</div>\
             <div class=\"lookout-card\"><h2 class=\"lookout-card-title\">Topic B</h2><p>second</p>\n<h3>Detail</h3>\n<p>third</p>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_gives_preamble_before_first_heading_its_own_untitled_card() {
        let cards = chat_reply_cards_html("Some intro text.\n\n## First section\nbody");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><p>Some intro text.</p>\n</div>\
             <div class=\"lookout-card\"><h2 class=\"lookout-card-title\">First section</h2><p>body</p>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_starts_directly_with_a_heading_without_an_empty_leading_card() {
        let cards = chat_reply_cards_html("# Only section\nbody");
        assert_eq!(cards, "<div class=\"lookout-card\"><h1 class=\"lookout-card-title\">Only section</h1><p>body</p>\n</div>");
    }

    #[test]
    fn chat_reply_cards_html_keeps_html_fence_graphics_passthrough_inside_a_card() {
        let cards = chat_reply_cards_html("## Chart\n```html\n<svg width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>\n```");
        assert_eq!(
            cards,
            "<div class=\"lookout-card\"><h2 class=\"lookout-card-title\">Chart</h2><svg width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>\n</div>"
        );
    }

    #[test]
    fn chat_reply_cards_html_handles_empty_reply() {
        assert_eq!(chat_reply_cards_html(""), "");
        assert_eq!(chat_reply_cards_html("   \n\n"), "");
    }

    #[test]
    fn chat_document_carries_the_card_styling_rules() {
        let doc = chat_document("");
        assert!(doc.contains(".lookout-card"));
        assert!(doc.contains(".lookout-card-title"));
    }

    #[test]
    fn chat_document_gives_links_a_high_contrast_colour() {
        let doc = chat_document("");
        assert!(
            doc.contains("a, a:visited { color: #4d9dff; }"),
            "links get an explicit accent colour instead of the low-contrast UA default"
        );
    }

    #[test]
    fn chat_document_leads_with_the_csp_meta_and_makes_the_background_transparent() {
        let doc = chat_document("<p>hi</p>");
        assert!(
            doc.starts_with("<meta http-equiv=\"Content-Security-Policy\" content=\""),
            "the CSP meta leads the document"
        );
        assert!(doc.contains("img-src * data: cid:"), "remote images are allowed at the chat level");
        assert!(doc.contains("script-src 'none'"), "scripts stay off");
        assert!(
            doc.contains("html, body { background: transparent; color: #e5e5e5; }"),
            "the transparent-background and grey-text rules are present"
        );
        assert!(doc.ends_with("</style><p>hi</p>"), "the style block rides in ahead of the content");
    }

    #[test]
    fn chat_empty_document_centres_the_robot_glyph_with_no_placeholder_text() {
        let doc = chat_empty_document();
        assert!(!doc.contains("reply appears here"), "the placeholder text is gone");
        assert!(doc.contains("class=\"chat-empty-icon\""), "the robot glyph is present");
        assert!(doc.contains("display: flex; align-items: center; justify-content: center"), "the glyph is centred");
        assert!(doc.contains("filter: invert(1)"), "the black-stroked artwork is lightened for the dark pane");
        assert!(doc.contains("stroke-opacity=\"0.9\""), "the artwork's own strokes ride along");
        assert!(doc.contains("viewBox=\"0 0 400 400\""), "the bundled ai-1.svg artwork is inlined");
        assert!(!doc.contains("<?xml"), "the XML declaration is stripped so the artwork embeds cleanly");
        assert!(
            doc.starts_with("<meta http-equiv=\"Content-Security-Policy\""),
            "the empty state loads through the same CSP document"
        );
    }

    #[test]
    fn chat_loading_document_pulses_the_robot_glyph() {
        let doc = chat_loading_document();
        assert!(doc.contains("class=\"chat-loading-icon\""), "the robot glyph is present");
        assert!(doc.contains("filter: invert(1)"), "the same lightening as the idle state");
        assert!(doc.contains("animation: chat-icon-pulse"), "the icon animates instead of sitting static");
        assert!(doc.contains("@keyframes chat-icon-pulse"), "the pulse keyframes are defined");
        assert!(doc.contains("viewBox=\"0 0 400 400\""), "the bundled ai-1.svg artwork is inlined");
        assert!(!doc.contains("<?xml"), "the XML declaration is stripped so the artwork embeds cleanly");
        assert!(
            doc.starts_with("<meta http-equiv=\"Content-Security-Policy\""),
            "the loading state loads through the same CSP document"
        );
    }

    #[test]
    fn chat_error_document_recolors_the_glyph_red_and_keeps_the_message() {
        let doc = chat_error_document("Failed: connection refused & timed out <retry>");
        assert!(doc.contains("class=\"chat-error-icon\""), "the robot glyph is present, recolored");
        assert!(doc.contains("background-color: #e01b24"), "the app's standard error red");
        assert!(
            doc.contains("mask-image: url(\"data:image/svg+xml;base64,"),
            "the artwork is masked, not filtered, for an exact color"
        );
        assert!(
            doc.contains("<p class=\"chat-error-message\">Failed: connection refused &amp; timed out &lt;retry&gt;</p>"),
            "the failure reason stays visible, HTML-escaped"
        );
        assert!(
            doc.starts_with("<meta http-equiv=\"Content-Security-Policy\""),
            "the error state loads through the same CSP document"
        );
    }
}
