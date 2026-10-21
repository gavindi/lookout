//! The message rows' drag-out to a file manager: a lazy content provider that writes the dragged messages' cached `.eml` files only when a drop target asks for them, on the blocking pool.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures_util::future::{FutureExt, LocalBoxFuture, Shared};
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use lookout_core::{EmailSummary, MailboxId, Uid, UidValidity};

use super::*;

/// How long the written `.eml` files outlive the drag. A file manager reads
/// the dropped URIs and finishes the drop before its copy job has run, so
/// deleting at drag end could pull the files out from under the copy.
const DRAG_FILE_GRACE_SECS: u32 = 60;

/// One dragged message, resolved on the GTK thread at drag start so the
/// write never needs `UiState`.
pub(crate) struct DragMessage {
    cache: Arc<lookout_mail::Cache>,
    mailbox: MailboxId,
    uid: Uid,
    uidvalidity: UidValidity,
}

/// Resolves each summary's account cache and folder `uidvalidity`. In-memory
/// lookups only; messages whose account isn't connected are left out.
pub(crate) fn drag_messages(state: &UiState, summaries: &[EmailSummary]) -> Vec<DragMessage> {
    summaries
        .iter()
        .filter_map(|summary| {
            let handle = state.accounts.get(&mailbox_account_id(&summary.mailbox)?)?;
            let cache = handle.address_cache.clone()?;
            let uidvalidity = handle.folders.iter().find(|f| f.id == summary.mailbox)?.uidvalidity;
            Some(DragMessage {
                cache,
                mailbox: summary.mailbox.clone(),
                uid: summary.uid,
                uidvalidity,
            })
        })
        .collect()
}

/// The private directory the drag's `.eml` files are written to:
/// `$XDG_RUNTIME_DIR/lookout/drag` (GLib falls back to the user cache dir),
/// never the shared temp dir.
pub(crate) fn drag_dir() -> PathBuf {
    glib::user_runtime_dir().join("lookout").join("drag")
}

/// Writes each message's raw bytes, when already in the flat-file cache, to a
/// fresh `0600` file in `dir` (created `0700`), and returns the written
/// paths. Messages not downloaded yet, or that fail to read or write, are
/// skipped. Blocking: runs on the blocking pool.
pub(crate) fn write_drag_files(dir: &Path, messages: &[DragMessage]) -> Vec<PathBuf> {
    if let Err(e) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        tracing::warn!("couldn't create the drag directory {}: {e}", dir.display());
        return Vec::new();
    }
    let mut paths = Vec::new();
    for message in messages {
        let Ok(Some(bytes)) = message.cache.load_raw_message(&message.mailbox, message.uid, message.uidvalidity) else {
            continue;
        };
        let path = dir.join(format!("lookout-drag-{}.eml", uuid::Uuid::new_v4()));
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(&bytes));
        match written {
            Ok(()) => paths.push(path),
            Err(e) => {
                tracing::warn!("couldn't write the dragged message to {}: {e}", path.display());
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    paths
}

/// Removes every leftover drag file: run at startup and shutdown, so files
/// still in their grace period when Lookout exits (or crashes) don't linger.
pub(crate) fn sweep_drag_dir() {
    let _ = std::fs::remove_dir_all(drag_dir());
}

fn remove_files(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

type WrittenFiles = Shared<LocalBoxFuture<'static, Rc<[PathBuf]>>>;

/// One drag's files: written at most once, on the first drop-target request,
/// and shared by every MIME type the target asks for. The row's drag source
/// keeps one alongside the provider and calls `finish` at drag end.
pub(crate) struct DragFiles {
    dir: PathBuf,
    /// Taken by the write when it starts.
    messages: RefCell<Vec<DragMessage>>,
    written: RefCell<Option<WrittenFiles>>,
    /// The files written so far, removed after the grace period by `finish`.
    paths: RefCell<Vec<PathBuf>>,
    finished: Cell<bool>,
}

impl DragFiles {
    pub(crate) fn new(messages: Vec<DragMessage>) -> Rc<Self> {
        Self::in_dir(drag_dir(), messages)
    }

    fn in_dir(dir: PathBuf, messages: Vec<DragMessage>) -> Rc<Self> {
        Rc::new(Self {
            dir,
            messages: RefCell::new(messages),
            written: RefCell::new(None),
            paths: RefCell::new(Vec::new()),
            finished: Cell::new(false),
        })
    }

    /// The written files, starting the write on the blocking pool the first
    /// time. A write that lands after `finish` (or after the drag was
    /// dropped altogether) removes its own files and resolves empty.
    fn written(self: &Rc<Self>) -> WrittenFiles {
        if let Some(written) = self.written.borrow().clone() {
            return written;
        }
        let messages = std::mem::take(&mut *self.messages.borrow_mut());
        let dir = self.dir.clone();
        let this = Rc::downgrade(self);
        let written = async move {
            let paths = gio::spawn_blocking(move || write_drag_files(&dir, &messages)).await.unwrap_or_default();
            match this.upgrade() {
                Some(files) if !files.finished.get() => {
                    files.paths.borrow_mut().extend(paths.iter().cloned());
                    Rc::from(paths)
                }
                _ => {
                    gio::spawn_blocking(move || remove_files(&paths));
                    Rc::from(Vec::new())
                }
            }
        }
        .boxed_local()
        .shared();
        *self.written.borrow_mut() = Some(written.clone());
        written
    }

    /// The drag has ended: removes the written files after the grace period.
    pub(crate) fn finish(&self) {
        self.finished.set(true);
        let paths = std::mem::take(&mut *self.paths.borrow_mut());
        if paths.is_empty() {
            return;
        }
        glib::timeout_add_seconds_local_once(DRAG_FILE_GRACE_SECS, move || {
            gio::spawn_blocking(move || remove_files(&paths));
        });
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct MessageFilesProvider {
        pub(super) files: RefCell<Option<Rc<DragFiles>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MessageFilesProvider {
        const NAME: &'static str = "LookoutMessageFilesProvider";
        type Type = super::MessageFilesProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for MessageFilesProvider {}

    impl ContentProviderImpl for MessageFilesProvider {
        /// The `FileList` type plus every MIME type GTK can serialize it to
        /// (`text/uri-list`, the Flatpak file-transfer portal's...). The MIME
        /// types must be listed explicitly: a union provider routes each
        /// write by `contain_mime_type`, and a GType alone matches none.
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormats::for_type(gdk::FileList::static_type()).union_serialize_mime_types()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), glib::Error>> + 'static>> {
            let files = self.files.borrow().clone();
            let mime_type = mime_type.to_string();
            let stream = stream.clone();
            Box::pin(async move {
                let paths = match files {
                    Some(files) => files.written().await,
                    None => Rc::from(Vec::new()),
                };
                if paths.is_empty() {
                    return Err(glib::Error::new(gio::IOErrorEnum::NotFound, "None of the dragged messages are downloaded yet"));
                }
                let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
                let value = gdk::FileList::from_array(&files).to_value();
                gdk::content_serialize_future(&stream, &mime_type, &value, io_priority).await
            })
        }

        /// Never synchronous: the files aren't written yet. `NotSupported`
        /// makes an in-process drop (the composer's attachment target) fall
        /// back to reading `text/uri-list` through `write_mime_type_future`.
        fn value(&self, _type: glib::Type) -> Result<glib::Value, glib::Error> {
            Err(glib::Error::new(gio::IOErrorEnum::NotSupported, "The dragged messages are only available as a stream"))
        }
    }
}

glib::wrapper! {
    /// The drag-out half of a message-row drag: offers the dragged messages
    /// as files, written lazily by its `DragFiles` when a target reads them.
    pub struct MessageFilesProvider(ObjectSubclass<imp::MessageFilesProvider>)
        @extends gdk::ContentProvider;
}

impl MessageFilesProvider {
    pub(crate) fn new(files: Rc<DragFiles>) -> Self {
        let provider: Self = glib::Object::new();
        *provider.imp().files.borrow_mut() = Some(files);
        provider
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use gtk::prelude::*;
    use lookout_core::AccountId;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lookout-drag-test-{name}-{}", uuid::Uuid::new_v4()))
    }

    /// A real on-disk mail cache with uid 1 downloaded and uid 2 not, and the
    /// two dragged messages pointing at them.
    fn cache_with_one_downloaded() -> (AccountId, Vec<DragMessage>) {
        let account_id = AccountId(format!("/test/drag_files_{}", uuid::Uuid::new_v4()));
        let cache = Arc::new(lookout_mail::Cache::open(&account_id).unwrap());
        let mailbox = MailboxId::new(&account_id, "INBOX");
        cache.store_raw_message(&mailbox, Uid(1), UidValidity(7), b"Subject: hi\r\n\r\nbody\r\n").unwrap();
        let messages = [1, 2]
            .into_iter()
            .map(|uid| DragMessage {
                cache: cache.clone(),
                mailbox: mailbox.clone(),
                uid: Uid(uid),
                uidvalidity: UidValidity(7),
            })
            .collect();
        (account_id, messages)
    }

    #[test]
    fn only_downloaded_messages_are_written_privately() {
        let (account_id, messages) = cache_with_one_downloaded();
        let dir = temp_dir("write");

        let paths = write_drag_files(&dir, &messages);

        assert_eq!(paths.len(), 1);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"Subject: hi\r\n\r\nbody\r\n");
        assert_eq!(std::fs::metadata(&paths[0]).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = lookout_mail::remove_account_cache(&account_id);
    }

    #[test]
    fn the_provider_serves_uris_lazily_and_never_synchronously() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let (account_id, messages) = cache_with_one_downloaded();
        let dir = temp_dir("provider");
        let provider = MessageFilesProvider::new(DragFiles::in_dir(dir.clone(), messages));

        let formats = provider.formats();
        assert!(formats.contains_type(gdk::FileList::static_type()));
        assert!(formats.contain_mime_type("text/uri-list"));
        assert!(!dir.exists(), "nothing is written before a target asks");
        let error = provider.value(gdk::FileList::static_type()).unwrap_err();
        assert!(error.matches(gio::IOErrorEnum::NotSupported));

        let stream = gio::MemoryOutputStream::new_resizable();
        glib::MainContext::default()
            .block_on(provider.write_mime_type_future("text/uri-list", &stream, glib::Priority::DEFAULT))
            .unwrap();
        stream.close(gio::Cancellable::NONE).unwrap();
        let uris = String::from_utf8(stream.steal_as_bytes().to_vec()).unwrap();
        let written: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().path()).collect();
        assert_eq!(written.len(), 1);
        assert!(uris.contains(gio::File::for_path(&written[0]).uri().as_str()));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = lookout_mail::remove_account_cache(&account_id);
    }

    #[test]
    fn a_write_landing_after_the_drag_ended_cleans_up() {
        let (account_id, messages) = cache_with_one_downloaded();
        let dir = temp_dir("late");
        let files = DragFiles::in_dir(dir.clone(), messages);

        let written = files.written();
        files.finish();
        let paths = glib::MainContext::new().block_on(written);

        assert!(paths.is_empty());
        assert!(files.paths.borrow().is_empty());
        // The late write's own cleanup also runs on the blocking pool.
        for _ in 0..100 {
            if std::fs::read_dir(&dir).map(|mut entries| entries.next().is_none()).unwrap_or(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = lookout_mail::remove_account_cache(&account_id);
    }
}
