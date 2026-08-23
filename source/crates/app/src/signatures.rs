//! The signature editor: Config → Accounts → "New Signature…" opens a modal
//! rich-text editor (the same contenteditable WebKit editor the composer
//! uses), and saving writes through to the shared `AppConfig` (and
//! `app_config::save`) and fires `on_changed` so the settings screen's
//! signature list can refresh - the same write-through pattern as the
//! manage-identities and manage-tags dialogs.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use base64::Engine;
use lookout_core::Signature;

use crate::app_config::AppConfig;

/// Presents the modal signature editor, anchored to `anchor`'s window when
/// one exists (tests have no window). `existing` prefills the editor (the
/// edit path); `None` starts a blank signature (the add path). `on_changed`
/// fires after every persisted change so live views of the signature list
/// can refresh.
pub fn show_editor_dialog(anchor: &gtk::Widget, app_config: Rc<RefCell<AppConfig>>, existing: Option<Signature>, on_changed: Rc<dyn Fn()>) {
    let dialog = {
        let mut builder = gtk::Window::builder()
            .modal(true)
            .title(if existing.is_some() { "Edit signature" } else { "New signature" })
            .default_width(720)
            .default_height(560);
        if let Some(win) = anchor.root().and_downcast::<gtk::Window>() {
            builder = builder.transient_for(&win);
        }
        builder.build()
    };

    let name_entry = gtk::Entry::builder()
        .placeholder_text("Name (e.g. Work, Personal)")
        .text(existing.as_ref().map(|s| s.name.as_str()).unwrap_or(""))
        .build();
    let (web_view, format_toolbar) = crate::compose::build_rich_editor(existing.as_ref().map(|s| s.html.clone()).unwrap_or_default());

    let toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    toolbar.append(&format_toolbar);
    let rich_scroller = gtk::ScrolledWindow::builder().child(&*web_view).hexpand(true).vexpand(true).build();

    let save_button = gtk::Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let cancel_button = gtk::Button::with_label("Cancel");
    let button_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).halign(gtk::Align::End).build();
    button_row.append(&cancel_button);
    button_row.append(&save_button);

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    root.append(&name_entry);
    root.append(&toolbar);
    root.append(&rich_scroller);
    root.append(&button_row);
    dialog.set_child(Some(&root));

    // Save stays disabled until a name is entered - the list rows are keyed
    // on the name, so an unnamed signature would be unreachable afterwards.
    save_button.set_sensitive(!name_entry.text().is_empty());
    {
        let save_button = save_button.clone();
        name_entry.connect_changed(move |entry| save_button.set_sensitive(!entry.text().is_empty()));
    }
    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }

    let edit_id = existing.as_ref().map(|s| s.id);
    let save = {
        let dialog = dialog.clone();
        let app_config = app_config.clone();
        let on_changed = on_changed.clone();
        let name_entry = name_entry.clone();
        let web_view = web_view.clone();
        move || {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let dialog = dialog.clone();
            let app_config = app_config.clone();
            let on_changed = on_changed.clone();
            let web_view = web_view.clone();
            // `read_content` must stay async (it awaits the WebView's JS
            // evaluation); see `compose::read_content` for why blocking is
            // unsafe inside a glib-spawned future.
            gtk::glib::spawn_future_local(async move {
                let Some(content) = crate::compose::read_content(&web_view).await else { return };
                let signature = Signature::new(name, embed_images(&content.html, &content.images), content.text);
                {
                    let mut config = app_config.borrow_mut();
                    if let Some(id) = edit_id {
                        config.upsert_signature(Signature { id, ..signature });
                    } else {
                        config.upsert_signature(signature);
                    }
                }
                crate::app_config::save(&app_config.borrow());
                on_changed();
                dialog.close();
            });
        }
    };
    let save_for_activate = save.clone();
    save_button.connect_clicked(move |_| save());
    name_entry.connect_activate(move |_| save_for_activate());

    dialog.present();
}

/// `read_content` pulls pasted images out of the editor as `cid:`-referenced
/// `InlineImage`s (the send path turns them into MIME attachments). A
/// signature must be self-contained - nothing attaches when a signature is
/// inserted - so the images are re-embedded as `data:` URIs right in the
/// stored HTML. The round trip stays intact: the composer's paste handler
/// renders `data:` images fine, and at send time `read_content` extracts
/// them back into inline attachments again.
fn embed_images(html: &str, images: &[lookout_mail::InlineImage]) -> String {
    if images.is_empty() {
        return html.to_string();
    }
    let mut out = html.to_string();
    for image in images {
        let data_uri = format!(
            "data:{};base64,{}",
            image.content_type,
            base64::engine::general_purpose::STANDARD.encode(&image.bytes)
        );
        out = out.replace(&format!("cid:{}", image.cid), &data_uri);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_images_turns_cid_references_into_data_uris() {
        let image = lookout_mail::InlineImage {
            cid: "img-1@lookout.local".to_string(),
            content_type: "image/png".to_string(),
            bytes: vec![0x89, 0x50, 0x4e, 0x47],
        };
        let html = r#"<p>Hi</p><img src="cid:img-1@lookout.local"><p>Bye</p>"#;
        let embedded = embed_images(html, &[image]);
        assert!(embedded.contains(r#"<img src="data:image/png;base64,iVBORw==">"#), "the cid reference must become an inline data URI");
        assert!(!embedded.contains("cid:"), "no cid reference may survive");
        assert!(embedded.contains("<p>Hi</p>"), "the surrounding HTML must be untouched");
    }

    #[test]
    fn embed_images_without_images_returns_the_html_unchanged() {
        let html = r#"<p>Hi</p><img src="cid:nope@lookout.local">"#;
        assert_eq!(embed_images(html, &[]), html);
    }
}
