//! Network work on background threads. Results go to the window through
//! a channel; a posted [`WM_APP_EVENT`] tells the window to read it.

use app_center::details;
use app_center::download;
use app_center::index::Package;
use app_center::manifest::Installer;
use app_center::select::Preferences;
use app_center::source::{Http, IndexProgress, Source};
use app_center::ui::app::Event;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to the window when events are waiting in the channel.
pub const WM_APP_EVENT: u32 = WM_APP + 1;

/// Progress is reported at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(120);

#[derive(Clone)]
struct Mailbox {
    hwnd: HWND,
    tx: Sender<Event>,
}

impl Mailbox {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            // SAFETY: PostMessageW may be called from any thread; a closed
            // window just makes it fail.
            unsafe {
                PostMessageW(self.hwnd, WM_APP_EVENT, 0, 0);
            }
        }
    }
}

/// Calls `f` at most every PROGRESS_EVERY, and always for the last call.
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    fn due(&mut self, last_one: bool) -> bool {
        let now = Instant::now();
        if last_one || self.last.is_none_or(|t| now - t >= PROGRESS_EVERY) {
            self.last = Some(now);
            true
        } else {
            false
        }
    }
}

pub struct Workers {
    mailbox: Mailbox,
    source: Arc<Source<Http>>,
    cancel: Arc<AtomicBool>,
}

impl Workers {
    pub fn new(hwnd: HWND, tx: Sender<Event>, source: Source<Http>) -> Workers {
        Workers {
            mailbox: Mailbox { hwnd, tx },
            source: Arc::new(source),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn load_index(&self, refresh: bool) {
        let (mailbox, source) = (self.mailbox.clone(), self.source.clone());
        std::thread::spawn(move || {
            let mut throttle = Throttle { last: None };
            let result = source.index_with_progress(refresh, &mut |p| {
                let last = match p {
                    IndexProgress::Downloading { done, total } => Some(done) == total,
                    _ => true,
                };
                if throttle.due(last) {
                    mailbox.send(Event::IndexProgress(p));
                }
            });
            mailbox.send(Event::IndexLoaded(result));
        });
    }

    pub fn fetch_details(&self, package: Package, prefs: Preferences) {
        let (mailbox, source) = (self.mailbox.clone(), self.source.clone());
        std::thread::spawn(move || {
            let result = details::fetch(&source, &package, None, &prefs);
            mailbox.send(Event::DetailsLoaded {
                id: package.id,
                result,
            });
        });
    }

    pub fn download(&self, id: String, installer: Installer, dir: PathBuf, fallback: String) {
        let (mailbox, source, cancel) = (
            self.mailbox.clone(),
            self.source.clone(),
            self.cancel.clone(),
        );
        cancel.store(false, Ordering::Relaxed);
        std::thread::spawn(move || {
            let mut throttle = Throttle { last: None };
            let result = download::download_cancellable(
                &source.fetch,
                &installer,
                &dir,
                &fallback,
                &mut |done, total| {
                    if throttle.due(Some(done) == total) {
                        mailbox.send(Event::DownloadProgress {
                            id: id.clone(),
                            done,
                            total,
                        });
                    }
                },
                Some(&cancel),
            );
            mailbox.send(Event::DownloadFinished { id, result });
        });
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
