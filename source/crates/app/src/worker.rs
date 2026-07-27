use std::future::Future;

/// Owns a dedicated OS thread running a tokio multi-thread runtime, kept
/// separate from the glib main loop that drives the GTK UI thread. IMAP/SMTP
/// and GOA D-Bus I/O all happen here; results/events cross back to the UI
/// thread over `async_channel`, consumed via `glib::spawn_future_local` (see
/// `window.rs`) - the standard gtk-rs pattern for combining a tokio reactor
/// with a glib main loop without running tokio inside it.
pub struct Worker {
    handle: tokio::runtime::Handle,
    _thread: std::thread::JoinHandle<()>,
}

impl Worker {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("lookout-worker".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime");
                tx.send(rt.handle().clone()).expect("UI thread dropped worker startup channel");
                rt.block_on(std::future::pending::<()>());
            })
            .expect("failed to spawn worker thread");
        let handle = rx.recv().expect("worker thread died before reporting its runtime handle");
        Worker { handle, _thread: thread }
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(future);
    }
}
