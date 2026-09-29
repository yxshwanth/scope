use std::error::Error;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use code_scope::index::{self, IndexReport};
use notify::{Event, EventKind, RecursiveMode, Watcher};

const QUIET_PERIOD: Duration = Duration::from_millis(250);
const MAX_BATCH: Duration = Duration::from_secs(2);
const RETRY_DELAY: Duration = Duration::from_secs(1);

pub fn run(root: &Path, mut on_update: impl FnMut(&IndexReport)) -> Result<(), Box<dyn Error>> {
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "watch requires a directory").into(),
        );
    }

    // One pending wakeup represents any number of filesystem changes.
    let (tx, rx) = mpsc::sync_channel(1);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stopped);
    let stop_tx = tx.clone();
    ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::Relaxed);
        let _ = stop_tx.try_send(());
    })?;

    let event_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        match result {
            Ok(event) if !needs_update(&event, &event_root) => return,
            Ok(_) => {}
            Err(error) => eprintln!("scope: watcher error: {error}; requesting a rescan"),
        }
        // Full means a rescan is already pending. Never block the callback.
        let _ = tx.try_send(());
    })?;

    // Subscribe first, so edits during the initial index enqueue another pass.
    watcher.watch(&root, RecursiveMode::Recursive)?;
    on_update(&index::rebuild(&root)?);
    eprintln!("watching {} (Ctrl+C to stop)", root.display());

    let mut retry = false;
    while !stopped.load(Ordering::Relaxed) {
        wait_for_change(&rx, &stopped, retry)?;
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "watched directory disappeared; restart watch at its current path",
            )
            .into());
        }

        match index::rebuild(&root) {
            Ok(report) => {
                on_update(&report);
                retry = false;
            }
            Err(error) => {
                eprintln!("scope: {error}; retrying in 1 second");
                retry = true;
            }
        }
    }

    // Dropping the watcher releases its operating system resources.
    drop(watcher);
    eprintln!("watch stopped");
    Ok(())
}

pub(crate) fn wait_for_change(
    rx: &mpsc::Receiver<()>,
    stopped: &AtomicBool,
    retry: bool,
) -> Result<(), mpsc::RecvTimeoutError> {
    if retry {
        match rx.recv_timeout(RETRY_DELAY) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error),
        }
    } else {
        rx.recv()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
    }
    let started = Instant::now();
    while !stopped.load(Ordering::Relaxed) {
        let remaining = MAX_BATCH.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(QUIET_PERIOD.min(remaining)) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn needs_update(event: &Event, root: &Path) -> bool {
    if event.need_rescan() {
        return true;
    }
    // Reading files during indexing must not trigger another index.
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.is_empty()
        || event.paths.iter().any(|path| {
            let Ok(relative) = path.strip_prefix(root) else {
                return true;
            };
            // This file changes Git's ignore rules.
            if relative == Path::new(".git/info/exclude") {
                return true;
            }
            !relative.components().any(|component| {
                matches!(
                    component.as_os_str().to_str(),
                    Some("target" | ".git" | "node_modules")
                )
            })
        })
}
