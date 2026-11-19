//! Small UI and text helpers shared across the app's views.
//!
//! Every helper here previously existed as a copy-pasted private function in
//! two or more view modules: HTML escaping for the WebView-rendered bodies
//! (compose, conversation, dashboard), and the recurring libadwaita widget
//! shapes (labelled rows, card sections, flat icon buttons, account labels).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use lookout_core::AccountId;

/// Escapes text for HTML: the five entities that matter for element text and
/// double-quoted attributes alike.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes a value for a double-quoted attribute: quotes and ampersands are
/// what matter, and angle brackets go too, so even a reader of the markup
/// can't mistake a body's `</iframe>` for structure.
pub fn escape_attribute(value: &str) -> String {
    escape_html(value)
}

/// Removes every child from a container (the common "repopulate this box"
/// prelude).
pub fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// Closes the popover `widget` sits inside (the nearest one, so a menu item
/// in a submenu or a nested popover closes its own).
///
/// For an item's click handler to call on the widget the signal hands it,
/// instead of capturing the popover: the popover owns the item, so a captured
/// strong handle is a reference cycle that keeps the whole menu alive after
/// its owner is destroyed (MEMORY_OPTIMISATION1.md, Phase 1).
pub fn popdown_enclosing_popover(widget: &impl IsA<gtk::Widget>) {
    if let Some(popover) = widget.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
        popover.popdown();
    }
}

/// One recorded handler: the object it is connected on, and its id.
type TrackedHandler = (glib::WeakRef<glib::Object>, glib::SignalHandlerId);
/// A clean-up [`HandlerSet::on_teardown`] runs once.
type Cleanup = Box<dyn FnOnce()>;

/// Signal handlers (and long-running tasks) that belong to one transient piece
/// of UI - a composer, an editor window - and are disconnected together when
/// it ends.
///
/// A handler connected on a widget that captures that widget, an ancestor of
/// it, or a struct holding either, is a reference cycle: GTK frees a widget
/// only when its last reference goes, and the handler only goes with the
/// widget. A form with dozens of such handlers would otherwise need weak
/// references threaded through each of them; recording them here and calling
/// [`HandlerSet::disconnect_all`] when the UI ends breaks every cycle at once
/// (MEMORY_OPTIMISATION1.md). Wrap each `connect_*` call in [`HandlerSet::track`].
#[derive(Clone, Default)]
pub(crate) struct HandlerSet {
    handlers: Rc<RefCell<Vec<TrackedHandler>>>,
    /// Long-running tasks to cancel too (an autosave tick, say, which would
    /// otherwise hold the form's fields until it next wakes).
    tasks: Rc<RefCell<Vec<glib::JoinHandle<()>>>>,
    /// Other clean-ups, run once: clearing app-level callback slots (not
    /// GObject signals) whose closures capture the UI's widgets.
    cleanups: Rc<RefCell<Vec<Cleanup>>>,
}

impl HandlerSet {
    /// Records a handler `object` connected, returning nothing so a
    /// `connect_*` call can be wrapped in place.
    pub(crate) fn track(&self, object: &impl IsA<glib::Object>, id: glib::SignalHandlerId) {
        let mut handlers = self.handlers.borrow_mut();
        // Widgets rebuilt while the UI is open (a list's rows) leave entries
        // whose object is gone; drop those rather than growing for good.
        handlers.retain(|(object, _)| object.upgrade().is_some());
        handlers.push((object.upcast_ref::<glib::Object>().downgrade(), id));
    }

    /// Records a task to cancel when the UI ends.
    pub(crate) fn track_task(&self, task: glib::JoinHandle<()>) {
        self.tasks.borrow_mut().push(task);
    }

    /// Records a clean-up to run when the UI ends.
    pub(crate) fn on_teardown(&self, cleanup: impl FnOnce() + 'static) {
        self.cleanups.borrow_mut().push(Box::new(cleanup));
    }

    /// Disconnects every recorded handler whose object is still alive, cancels
    /// the recorded tasks and runs the clean-ups. Safe to call more than once, and from
    /// inside one of the handlers (GLib keeps a running handler's closure
    /// until it returns). A handler must not also disconnect itself, or this
    /// would disconnect a stale id.
    pub(crate) fn disconnect_all(&self) {
        let handlers = std::mem::take(&mut *self.handlers.borrow_mut());
        for (object, id) in handlers {
            if let Some(object) = object.upgrade() {
                object.disconnect(id);
            }
        }
        for task in std::mem::take(&mut *self.tasks.borrow_mut()) {
            task.abort();
        }
        let cleanups = std::mem::take(&mut *self.cleanups.borrow_mut());
        for cleanup in cleanups {
            cleanup();
        }
    }
}

/// A flat button with a themed icon and a label, as the reading pane's
/// header actions and the profile card use.
pub fn action_button(icon: &str, label: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder().icon_name(icon).label(label).build();
    gtk::Button::builder().child(&content).css_classes(["flat"]).build()
}

/// A caption-plus-widget form row (the settings-style dialog fields).
pub fn labeled_row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    let label = gtk::Label::builder().label(label).width_chars(9).halign(gtk::Align::Start).build();
    crate::privacy::mark_ui(&label);
    widget.set_hexpand(true);
    row.append(&label);
    row.append(widget);
    row
}

/// Wraps content in a `.card`-styled box - libadwaita's built-in style class
/// for a rounded-corner, softly-shaded panel. No per-section header bar: the
/// enclosing `Gtk.Paned` (or dialog) provides the gap between cards, and the
/// small margin here is what makes each card's rounded corners visible
/// against the window background instead of touching the divider/edges.
pub fn card_section(content: &impl IsA<gtk::Widget>) -> gtk::Box {
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

/// The best available human name for an account: its display name, else its
/// email, else the raw account id.
pub fn account_label(account: &AccountId, display_name: &str, email: &str) -> String {
    if !display_name.trim().is_empty() {
        display_name.to_string()
    } else if !email.trim().is_empty() {
        email.to_string()
    } else {
        account.0.clone()
    }
}

/// Runs a repaint at once while `widget` is on screen, and otherwise holds the
/// latest request until the widget is next mapped. For views whose content is
/// rebuilt on every sync but only matters while shown: the People list built
/// a row, avatar and contact copies per contact while People was hidden
/// (MEMORY_OPTIMISATION1.md, "Per-account breakdown").
pub struct DeferUntilMapped<T: 'static> {
    inner: Rc<DeferInner<T>>,
}

struct DeferInner<T> {
    widget: glib::WeakRef<gtk::Widget>,
    pending: RefCell<Option<T>>,
    /// Folds a new request into a pending one.
    merge: fn(Option<T>, T) -> T,
    run: Box<dyn Fn(T)>,
}

impl<T: 'static> DeferUntilMapped<T> {
    /// `run` does the repaint; `merge` folds a request made while the widget
    /// is hidden into the one already pending.
    pub fn new(widget: &impl IsA<gtk::Widget>, merge: fn(Option<T>, T) -> T, run: impl Fn(T) + 'static) -> Self {
        let inner = Rc::new(DeferInner {
            widget: widget.upcast_ref::<gtk::Widget>().downgrade(),
            pending: RefCell::new(None),
            merge,
            run: Box::new(run),
        });
        // Weak: `run` usually holds widgets inside `widget`.
        let weak = Rc::downgrade(&inner);
        widget.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.flush();
            }
        });
        DeferUntilMapped { inner }
    }

    /// Repaints now if the widget is mapped, else remembers `value` for when
    /// it is.
    pub fn request(&self, value: T) {
        let Some(widget) = self.inner.widget.upgrade() else { return };
        if widget.is_mapped() {
            self.inner.pending.borrow_mut().take();
            (self.inner.run)(value);
        } else {
            let previous = self.inner.pending.borrow_mut().take();
            let merged = (self.inner.merge)(previous, value);
            *self.inner.pending.borrow_mut() = Some(merged);
        }
    }

    /// Whether a request is waiting for the widget to be mapped.
    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.inner.pending.borrow().is_some()
    }
}

impl<T> DeferInner<T> {
    fn flush(&self) {
        let pending = self.pending.borrow_mut().take();
        if let Some(value) = pending {
            (self.run)(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// While the widget is hidden, requests are held (merged) and run once
    /// it is mapped; while it is shown they run at once.
    #[test]
    fn defer_until_mapped_runs_on_map_and_merges_while_hidden() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        fn settle(until: impl Fn() -> bool) {
            let context = glib::MainContext::default();
            for _ in 0..200 {
                while context.iteration(false) {}
                if until() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let runs: Rc<RefCell<Vec<Option<i32>>>> = Rc::new(RefCell::new(Vec::new()));
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let window = gtk::Window::builder().child(&widget).build();
        // The People rule: a background `None` doesn't replace an explicit index.
        let keep_explicit = |previous: Option<Option<i32>>, new: Option<i32>| match (previous, new) {
            (Some(Some(index)), None) => Some(index),
            (_, new) => new,
        };
        let defer = {
            let runs = runs.clone();
            DeferUntilMapped::new(&widget, keep_explicit, move |value| runs.borrow_mut().push(value))
        };
        defer.request(Some(3));
        defer.request(None);
        assert!(runs.borrow().is_empty(), "nothing runs while hidden");
        assert!(defer.is_pending());

        window.present();
        settle(|| widget.is_mapped());
        assert!(widget.is_mapped(), "the window maps");
        assert_eq!(*runs.borrow(), [Some(3)], "the held request runs on map, its explicit index kept");
        assert!(!defer.is_pending());
        defer.request(Some(5));
        assert_eq!(*runs.borrow(), [Some(3), Some(5)], "runs at once while shown");

        window.set_visible(false);
        settle(|| !widget.is_mapped());
        defer.request(None);
        assert_eq!(runs.borrow().len(), 2, "held again once hidden");
        window.present();
        settle(|| widget.is_mapped());
        assert_eq!(*runs.borrow(), [Some(3), Some(5), None]);
        window.destroy();
    }

    #[test]
    fn escape_html_covers_text_and_attribute_metacharacters() {
        assert_eq!(escape_html("a<b>&\"c\"'d'"), "a&lt;b&gt;&amp;&quot;c&quot;&#39;d&#39;");
        assert_eq!(escape_attribute("</iframe>"), "&lt;/iframe&gt;");
    }

    #[test]
    fn account_label_prefers_name_then_email_then_id() {
        let account = AccountId("acct-1".to_string());
        assert_eq!(account_label(&account, "Ada", "ada@example.com"), "Ada");
        assert_eq!(account_label(&account, "  ", "ada@example.com"), "ada@example.com");
        assert_eq!(account_label(&account, "", ""), "acct-1");
    }
}
