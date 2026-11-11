//! Change detection: an inotify watch over the applications directories, the
//! desktop folder, the config directory and the reload trigger file, so the
//! menu refreshes itself when an application is installed or removed.

use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config;
use crate::entries;

/// How long after the last change the reload runs, so a package installing
/// many files causes one reload rather than one per file.
const SETTLE: Duration = Duration::from_millis(400);

/// The legacy trigger file; touching it forces a reload.
pub fn trigger_file() -> PathBuf {
    config::home().join(".config/panel-reload")
}

/// The inotify watch set.
pub struct Watcher {
    fd: RawFd,
    pending_since: Option<Instant>,
}

impl Watcher {
    /// Creates the inotify instance and adds every watch.  Directories that
    /// do not exist yet are skipped; a reload re-adds them.
    pub fn new() -> Watcher {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        let mut watcher = Watcher {
            fd,
            pending_since: None,
        };
        watcher.add_all();
        watcher
    }

    /// Adds the watches for every directory of interest.
    fn add_all(&mut self) {
        if self.fd < 0 {
            return;
        }
        let trigger = trigger_file();
        if let Some(parent) = trigger.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if !trigger.exists() {
            let _ = std::fs::write(&trigger, b"");
        }
        self.watch(&trigger, libc::IN_ATTRIB | libc::IN_MODIFY | libc::IN_CLOSE_WRITE);
        let dir_mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB;
        for dir in entries::applications_dirs() {
            self.watch(&dir, dir_mask);
        }
        self.watch(&entries::desktop_dir(), dir_mask);
        let config_dir = config::config_dir();
        let _ = std::fs::create_dir_all(&config_dir);
        self.watch(&config_dir, dir_mask);
    }

    /// Adds one watch.  A path that does not exist yet has its nearest
    /// existing ancestor watched for its creation instead, so the reload that
    /// follows picks the new directory up.
    fn watch(&self, path: &Path, mask: u32) {
        let mut target = path;
        let mut mask = mask;
        while !target.exists() {
            let Some(parent) = target.parent() else {
                return;
            };
            target = parent;
            mask = libc::IN_CREATE | libc::IN_MOVED_TO;
        }
        let Ok(c_path) = CString::new(target.as_os_str().as_bytes()) else {
            return;
        };
        unsafe {
            libc::inotify_add_watch(self.fd, c_path.as_ptr(), mask);
        }
    }

    /// The descriptor to poll for readability.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Drains the queued notifications and starts the settle timer.
    pub fn drain(&mut self) {
        if self.fd < 0 {
            return;
        }
        let mut buf = [0u8; 4096];
        loop {
            let read = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if read <= 0 {
                break;
            }
            self.pending_since = Some(Instant::now());
        }
    }

    /// Whether a reload is due, and clears the pending change when it is.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.pending_since {
            Some(since) if now.duration_since(since) >= SETTLE => {
                self.pending_since = None;
                self.add_all();
                true
            }
            _ => false,
        }
    }

    /// When the settle timer fires, if a change is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending_since.map(|since| since + SETTLE)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}
