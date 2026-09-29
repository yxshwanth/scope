use std::error::Error;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use code_scope::{Symbol, index};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::watch::{needs_update, wait_for_change};

pub enum Update {
    Indexing,
    Ready(index::IndexReport, Vec<Symbol>),
    Failed(String),
}

pub struct LiveIndex {
    requests: mpsc::SyncSender<()>,
    updates: Option<mpsc::Receiver<Update>>,
    stopped: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
    worker: Option<JoinHandle<()>>,
}

impl LiveIndex {
    pub fn start(root: &Path) -> Result<Self, Box<dyn Error>> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "browse requires a directory").into(),
            );
        }
        let (requests, work) = mpsc::sync_channel(1);
        let (results, updates) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let watch_error = Arc::new(Mutex::new(None));

        let event_root = root.clone();
        let event_requests = requests.clone();
        let event_error = Arc::clone(&watch_error);
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            match result {
                Ok(event) if !needs_update(&event, &event_root) => return,
                Ok(_) => {}
                Err(error) => {
                    if let Ok(mut slot) = event_error.lock() {
                        *slot = Some(error.to_string());
                    }
                }
            }
            let _ = event_requests.try_send(());
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;

        let stop = Arc::clone(&stopped);
        let worker = thread::Builder::new()
            .name("scope-index".to_owned())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if results.send(Update::Indexing).is_err() {
                        break;
                    }
                    let error = watch_error.lock().ok().and_then(|mut slot| slot.take());
                    if let Some(error) = error
                        && results
                            .send(Update::Failed(format!("Watcher: {error}; rescanning")))
                            .is_err()
                    {
                        break;
                    }
                    // This thread owns SQLite; parsing uses Rayon's worker pool.
                    let next = index::rebuild(&root).and_then(|report| {
                        index::find(&root, "").map(|symbols| Update::Ready(report, symbols))
                    });
                    let retry = next.is_err();
                    let update = next.unwrap_or_else(|error| Update::Failed(error.to_string()));
                    if results.send(update).is_err() || stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if wait_for_change(&work, &stop, retry).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests,
            updates: Some(updates),
            stopped,
            watcher: Some(watcher),
            worker: Some(worker),
        })
    }

    pub fn request(&self) {
        let _ = self.requests.try_send(());
    }

    pub fn try_next(&self) -> io::Result<Option<Update>> {
        match self
            .updates
            .as_ref()
            .expect("receiver exists until drop")
            .try_recv()
        {
            Ok(update) => Ok(Some(update)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(io::Error::other("index worker stopped unexpectedly"))
            }
        }
    }
}

impl Drop for LiveIndex {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.request();
        // Disconnect before joining: the worker may be blocked sending a result.
        self.updates.take();
        self.watcher.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn drop_unblocks_a_worker_with_a_full_result_channel() {
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let (results, updates) = mpsc::sync_channel(1);
            results.send(Update::Indexing).unwrap();
            let worker = thread::spawn(move || {
                let _ = results.send(Update::Failed(String::new()));
            });
            let (requests, _work) = mpsc::sync_channel(1);
            drop(LiveIndex {
                requests,
                updates: Some(updates),
                stopped: Arc::new(AtomicBool::new(false)),
                watcher: None,
                worker: Some(worker),
            });
            done.send(()).unwrap();
        });
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdown deadlocked");
    }
}
