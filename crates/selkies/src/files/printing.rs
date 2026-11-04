/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Documents printed in the session, on their way to a browser.
//!
//! The session's print queue, Selkies, is a CUPS scheduler run as this user whose backend
//! writes each job as a PDF into the print spool. The spool is watched, every document that
//! lands whole is announced to the controller pages of the active transport, and a page
//! fetches it once through `GET /api/print/{name}`, which takes it out of the spool. The spool
//! therefore holds what no page has taken yet, and a page connecting later is told about that
//! too ([`pending`]).
//!
//! [`SpoolWatcher`] reads inotify on the spool from a thread of its own: a document renamed
//! into place or closed after being written there is whole at that event; one that only
//! appears is announced once it has sat unopened for [`SETTLE_SECONDS`], which is what tells
//! it apart from a file still being written. [`PrintQueue`] runs a copy of the installed
//! `cupsd` under the runtime directory with its whole state there, so nothing under `/etc/cups`
//! is touched and no privilege is needed; applications reach it with `CUPS_SERVER` set to its
//! socket. The scheduler is asked to die with this process (`PR_SET_PDEATHSIG`): one killed
//! outright would otherwise leave the scheduler holding the socket and the program for the
//! next start to trip over.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use nix::unistd::AccessFlags;

use super::transfer::{FileServe, audited_file_response};
use super::{FilesState, RequestAuth, text};

/// How long a file that merely appeared in the spool is left alone before it counts as whole:
/// one moved in from elsewhere is complete at once, one being written in place is opened or
/// modified well within this.
pub const SETTLE_SECONDS: f64 = 0.5;

/// The queue's CUPS backend, written into the scheduler's private tree.
const BACKEND: &[u8] = include_bytes!("../../assets/cups/backend");
/// The queue's PPD, as shipped (A4).
const PPD: &str = include_str!("../../assets/cups/selkies.ppd");

/// The print spool: `configured` when set (`~` expanded), else `selkies/print` under the XDG
/// state directory, as the server's other state is.
pub fn spool_path(configured: &str) -> PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty());
    spool_path_in(
        configured,
        state_home.as_deref(),
        &std::env::var("HOME").unwrap_or_default(),
    )
}

/// [`spool_path`] against an explicit `XDG_STATE_HOME` and `HOME`.
pub fn spool_path_in(configured: &str, state_home: Option<&str>, home: &str) -> PathBuf {
    if !configured.is_empty() {
        return super::expand_user(configured);
    }
    match state_home {
        Some(state) => Path::new(state).join("selkies").join("print"),
        None => Path::new(home).join(".local/state/selkies/print"),
    }
}

/// `name` when it names a document in the spool, else `None`: a path, a hidden or partial
/// file, or anything that is not a PDF is refused.
pub fn document_name(name: &str) -> Option<&str> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') || name.contains('\0') {
        return None;
    }
    name.to_lowercase().ends_with(".pdf").then_some(name)
}

/// Every document still in the spool with its size, oldest first.
pub fn pending(spool: &Path) -> Vec<(String, u64)> {
    let Ok(listing) = std::fs::read_dir(spool) else {
        return Vec::new();
    };
    let mut entries: Vec<(i128, String, u64)> = listing
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            document_name(&name)?;
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
            Some((mtime, name, meta.len()))
        })
        .collect();
    entries.sort();
    entries
        .into_iter()
        .map(|(_, name, size)| (name, size))
        .collect()
}

/// `ppd` with its default paper set to `requested`, and the size it now defaults to, or
/// `None` where `requested` names no size the PPD offers.
///
/// `requested` is a paper name as libpaper's `PAPERSIZE` gives it (`letter`, `a4`), matched
/// against the PPD's sizes ignoring case. Only that explicit choice moves the default: a
/// host's `/etc/papersize` and locale are left unread, because a container image sets both as
/// an accident of its build rather than for the person printing, so following them would
/// change the page for users nobody asked about.
pub fn default_paper(ppd: &str, requested: &str) -> (String, Option<String>) {
    let sizes = regex::Regex::new(r"(?m)^\*PageSize (\w+)/").expect("static pattern");
    let wanted = requested.trim().to_lowercase();
    let size = sizes
        .captures_iter(ppd)
        .map(|c| c[1].to_string())
        .find(|s| s.to_lowercase() == wanted);
    let Some(size) = size else {
        return (ppd.to_string(), None);
    };
    let defaults = regex::Regex::new(
        r"(?m)^(\*Default(?:PageSize|PageRegion|ImageableArea|PaperDimension):) \S+",
    )
    .expect("static pattern");
    let text = defaults
        .replace_all(ppd, |c: &regex::Captures| format!("{} {size}", &c[1]))
        .into_owned();
    (text, Some(size))
}

/// What the watcher tells its owner: a document landed whole in the spool.
pub type OnDocument = Arc<dyn Fn(&str, u64) + Send + Sync>;

/// Announces each document that lands whole in the spool (module docs).
pub struct SpoolWatcher {
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    task: tokio::task::JoinHandle<()>,
}

/// The inotify events the watcher reads: a name created, moved in, opened, modified, or
/// closed after writing.
const WATCH_MASK: u32 =
    libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_OPEN | libc::IN_MODIFY | libc::IN_CLOSE_WRITE;

impl SpoolWatcher {
    /// Watch `spool`, creating it where missing, and call `on_document` for each whole
    /// document; fails where inotify cannot watch the directory.
    pub fn start(spool: PathBuf, on_document: OnDocument) -> std::io::Result<SpoolWatcher> {
        std::fs::create_dir_all(&spool)?;
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let c_spool = CString::new(spool.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "spool path holds a NUL")
        })?;
        if unsafe { libc::inotify_add_watch(fd, c_spool.as_ptr(), WATCH_MASK) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(u32, String)>();
        let reader = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("selkies-spool".into())
                .spawn(move || read_events(fd, &stop, &tx))?
        };
        let task = tokio::spawn(announce_loop(rx, spool, on_document));
        Ok(SpoolWatcher {
            stop,
            reader: Some(reader),
            task,
        })
    }

    /// Stop watching; the reader thread ends within its poll interval.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for SpoolWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

/// The reader thread: poll the inotify descriptor, parse each event into its mask and name,
/// and send them on; closes the descriptor when told to stop or when the receiver is gone.
fn read_events(fd: i32, stop: &AtomicBool, tx: &tokio::sync::mpsc::UnboundedSender<(u32, String)>) {
    let mut buf = vec![0u8; 64 * 1024];
    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
        if ready <= 0 {
            continue;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            continue;
        }
        let mut offset = 0usize;
        let header = std::mem::size_of::<libc::inotify_event>();
        while offset + header <= n as usize {
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast()) };
            let name_start = offset + header;
            let name_end = (name_start + event.len as usize).min(n as usize);
            let name_bytes = &buf[name_start..name_end];
            let name = name_bytes
                .iter()
                .position(|b| *b == 0)
                .map_or(name_bytes, |nul| &name_bytes[..nul]);
            let name = String::from_utf8_lossy(name).into_owned();
            if !name.is_empty() && tx.send((event.mask, name)).is_err() {
                unsafe { libc::close(fd) };
                return;
            }
            offset = name_end;
        }
    }
    unsafe { libc::close(fd) };
}

/// The announcing task: a document moved in or closed after writing is announced at once; one
/// merely created is announced after [`SETTLE_SECONDS`] unless it is opened or modified first.
async fn announce_loop(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(u32, String)>,
    spool: PathBuf,
    on_document: OnDocument,
) {
    let mut settling: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    while let Some((mask, name)) = rx.recv().await {
        if mask & libc::IN_ISDIR != 0 {
            continue;
        }
        if let Some(timer) = settling.remove(&name) {
            timer.abort();
        }
        if mask & (libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE) != 0 {
            announce(&spool, &name, &on_document);
        } else if mask & libc::IN_CREATE != 0 {
            let (spool, on_document, settled_name) =
                (spool.clone(), on_document.clone(), name.clone());
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(SETTLE_SECONDS)).await;
                announce(&spool, &settled_name, &on_document);
            });
            settling.insert(name, timer.abort_handle());
        }
    }
}

/// Announce `name` where it is a document still in the spool.
fn announce(spool: &Path, name: &str, on_document: &OnDocument) {
    if document_name(name).is_none() {
        return;
    }
    let Ok(meta) = std::fs::metadata(spool.join(name)) else {
        return;
    };
    on_document(name, meta.len());
}

/// `GET /api/print/{name}`: hand a printed document to the page that prints it, and take it
/// out of the spool once it went out whole. Refused to view-only credentials, since printing
/// is the session owner's act, and to another site's page, which would take the document out
/// of the spool with the session's cached login.
pub async fn handle_print_document(
    State(state): State<Arc<FilesState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let auth = RequestAuth::of(request.extensions());
    if !state.config.printing_enabled {
        return text(StatusCode::FORBIDDEN, "Forbidden: printing disabled");
    }
    if auth.fetched_cross_site(request.headers()) {
        log::warn!(
            target: "printing",
            "Refused a printed document to another site's page (Origin {:?})",
            request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("")
        );
        return text(StatusCode::FORBIDDEN, "Forbidden origin");
    }
    if auth.viewer_ceiling {
        return text(
            StatusCode::FORBIDDEN,
            "View-only credentials cannot take printed documents",
        );
    }
    let Some(name) = document_name(&name) else {
        return text(StatusCode::NOT_FOUND, "No such document");
    };
    let full = state.print_spool.join(name);
    let mut prefix = state.print_spool.as_os_str().to_os_string();
    prefix.push("/");
    if !full.as_os_str().as_bytes().starts_with(prefix.as_bytes()) || !full.is_file() {
        return text(StatusCode::NOT_FOUND, "No such document");
    }
    let serve = FileServe {
        path: &full,
        served_name: name,
        event: "print.document",
        remove: true,
        extra: vec![(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("inline"),
        )],
    };
    audited_file_response(&state, request.method(), request.headers(), serve).await
}

/// The first `name` on the `:`-separated `path` the caller may `access` with `mode`: a file, never
/// a directory.
fn which_in(path: &str, name: &str, mode: AccessFlags) -> Option<PathBuf> {
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|candidate| candidate.is_file() && nix::unistd::access(candidate, mode).is_ok())
}

/// The session's print queue (module docs).
pub struct PrintQueue {
    /// The scheduler's state root.
    pub root: PathBuf,
    /// The socket applications print to.
    pub socket: PathBuf,
    /// Where the backend writes finished jobs.
    pub spool: PathBuf,
    /// The paper the queue defaults to.
    pub paper: String,
    process: Option<tokio::process::Child>,
}

impl PrintQueue {
    /// Where the scheduler is looked for, beyond `PATH`: the sbin directories a session's
    /// `PATH` usually leaves out.
    pub const SEARCH_PATH: &'static str = ":/usr/sbin:/usr/local/sbin";

    /// A queue spooling to `spool`, its state under `$XDG_RUNTIME_DIR/selkies-cups`, or
    /// `selkies-cups-<uid>` in the temporary directory without a runtime directory.
    pub fn new(spool: PathBuf) -> Self {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|r| !r.is_empty());
        Self::with_root(
            spool,
            Self::state_root(runtime.as_deref(), &std::env::temp_dir()),
        )
    }

    /// The state root for a runtime directory, or for the temporary directory without one.
    pub fn state_root(runtime_dir: Option<&str>, temp_dir: &Path) -> PathBuf {
        match runtime_dir {
            Some(runtime) => Path::new(runtime).join("selkies-cups"),
            None => temp_dir.join(format!("selkies-cups-{}", nix::unistd::getuid().as_raw())),
        }
    }

    /// A queue with its state under `root`.
    pub fn with_root(spool: PathBuf, root: PathBuf) -> Self {
        PrintQueue {
            socket: root.join("cups.sock"),
            root,
            spool,
            paper: "A4".into(),
            process: None,
        }
    }

    /// `(cupsd, server bin, data dir)` of the installed CUPS, or `None`.
    pub fn programs() -> Option<(PathBuf, PathBuf, PathBuf)> {
        Self::programs_on(&format!(
            "{}{}",
            std::env::var("PATH").unwrap_or_default(),
            Self::SEARCH_PATH
        ))
    }

    /// [`PrintQueue::programs`] over an explicit search path.
    pub fn programs_on(path: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let cupsd = which_in(path, "cupsd", AccessFlags::X_OK)?;
        let prefix = std::fs::canonicalize(&cupsd)
            .ok()?
            .parent()?
            .parent()?
            .to_path_buf();
        for lib in ["lib", "lib64", "libexec"] {
            let server_bin = prefix.join(lib).join("cups");
            if server_bin.join("daemon/cups-exec").is_file() && server_bin.join("filter").is_dir() {
                return Some((cupsd, server_bin, prefix.join("share/cups")));
            }
        }
        None
    }

    /// `(path, mode)` of a `cupsd` on the search path that this user can neither read nor
    /// run, which [`PrintQueue::programs`] passes over, or `None`. The queue runs a copy of the
    /// program as the session user, so a scheduler installed for root alone cannot serve it
    /// however it is configured.
    pub fn locked_scheduler() -> Option<(PathBuf, u32)> {
        Self::locked_scheduler_on(&format!(
            "{}{}",
            std::env::var("PATH").unwrap_or_default(),
            Self::SEARCH_PATH
        ))
    }

    /// [`PrintQueue::locked_scheduler`] over an explicit search path.
    pub fn locked_scheduler_on(path: &str) -> Option<(PathBuf, u32)> {
        let cupsd = which_in(path, "cupsd", AccessFlags::F_OK)?;
        if nix::unistd::access(&cupsd, AccessFlags::R_OK | AccessFlags::X_OK).is_ok() {
            return None;
        }
        let mode = std::fs::metadata(&cupsd).ok()?.permissions().mode() & 0o7777;
        Some((cupsd, mode))
    }

    /// Make the state root this user's alone, or refuse it: without a runtime directory it
    /// sits in the shared temporary one under a predictable name, where another account could
    /// create it first and plant the programs and configuration the scheduler then runs as
    /// this user.
    pub fn claim_root(&self) -> std::io::Result<()> {
        match nix::unistd::mkdir(&self.root, nix::sys::stat::Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
            Err(e) => return Err(e.into()),
        }
        let st = std::fs::symlink_metadata(&self.root)?;
        if !st.is_dir() || st.uid() != nix::unistd::getuid().as_raw() || st.mode() & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a directory this user alone owns and writes",
                    self.root.display()
                ),
            ));
        }
        Ok(())
    }

    /// Lay the scheduler's tree out under the root: its directories, the links to the
    /// installed filters and daemons, the backend, the PPD on the requested paper, and the
    /// configuration naming them.
    fn prepare(&mut self, server_bin: &Path, data_dir: &Path) -> std::io::Result<()> {
        self.claim_root()?;
        for sub in ["ppd", "state", "cache", "spool", "tmp", "bin/backend"] {
            std::fs::create_dir_all(self.root.join(sub))?;
        }
        std::fs::create_dir_all(&self.spool)?;
        for sub in ["filter", "daemon"] {
            let link = self.root.join("bin").join(sub);
            if std::fs::symlink_metadata(&link)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                std::fs::remove_file(&link)?;
            }
            std::os::unix::fs::symlink(server_bin.join(sub), &link)?;
        }
        let backend = self.root.join("bin/backend/selkies");
        std::fs::write(&backend, BACKEND)?;
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o755))?;
        let requested = std::env::var("PAPERSIZE").unwrap_or_default();
        let (ppd, size) = default_paper(PPD, &requested);
        if !requested.is_empty() && size.is_none() {
            log::warn!(target: "printing", "PAPERSIZE={requested} names no size the Selkies queue offers; it stays on A4");
        }
        self.paper = size.unwrap_or_else(|| "A4".into());
        std::fs::write(self.root.join("ppd/Selkies.ppd"), ppd)?;
        let root = self.root.display();
        let files_conf = format!(
            "ServerRoot {root}\nServerBin {root}/bin\nDataDir {}\nStateDir {root}/state\nCacheDir {root}/cache\nRequestRoot {root}/spool\nTempDir {root}/tmp\nLogFileGroup {}\nAccessLog {root}/access.log\nPageLog {root}/page.log\nErrorLog {root}/error.log\n",
            data_dir.display(),
            nix::unistd::getgid().as_raw()
        );
        let cupsd_conf = format!(
            "Listen {}\nLogLevel warn\nBrowsing Off\nWebInterface No\n",
            self.socket.display()
        );
        let printers_conf = format!(
            "<DefaultPrinter Selkies>\nInfo Selkies printer\nLocation In the browser\nDeviceURI selkies:{}\nState Idle\nAccepting Yes\nShared No\nJobSheets none none\nErrorPolicy retry-job\n</DefaultPrinter>\n",
            self.spool.display()
        );
        std::fs::write(self.root.join("cups-files.conf"), files_conf)?;
        std::fs::write(self.root.join("cupsd.conf"), cupsd_conf)?;
        std::fs::write(self.root.join("printers.conf"), printers_conf)?;
        if self.socket.exists() {
            std::fs::remove_file(&self.socket)?;
        }
        Ok(())
    }

    /// Start the scheduler; `false` where CUPS is not installed or the queue did not come up.
    ///
    /// The scheduler runs as a copy of the program: a distribution confines the system
    /// scheduler, by its path, to the system's directories and backends, which a queue under
    /// the runtime directory has neither of. The copy is renamed into place, which a scheduler
    /// still running the old copy (left by a server that died without stopping it) never
    /// blocks the way writing over its program would.
    pub async fn start(&mut self) -> bool {
        let Some((cupsd, server_bin, data_dir)) = Self::programs() else {
            match Self::locked_scheduler() {
                Some((path, mode)) => log::warn!(
                    target: "printing",
                    "No print queue: {} is mode {mode:o}, which this user can neither read nor run; a Selkies queue needs the scheduler readable (Arch's cups package installs it for root alone)",
                    path.display()
                ),
                None => log::info!(
                    target: "printing",
                    "No CUPS scheduler on this host: documents printed into the spool are still handed over, but there is no Selkies queue to print to (cups-daemon and cups-filters provide one)"
                ),
            }
            return false;
        };
        let program = self.root.join("cupsd");
        let staged = self.root.join("cupsd.new");
        let prepared = self.prepare(&server_bin, &data_dir).and_then(|_| {
            std::fs::copy(&cupsd, &staged)?;
            std::fs::rename(&staged, &program)
        });
        if let Err(e) = prepared {
            log::warn!(target: "printing", "No print queue: cannot set the scheduler up under {}: {e}", self.root.display());
            return false;
        }
        let args = [
            OsStr::new("-f"),
            OsStr::new("-c"),
            self.root.join("cupsd.conf").as_os_str(),
            OsStr::new("-s"),
            self.root.join("cups-files.conf").as_os_str(),
        ]
        .map(|a| a.to_os_string());
        let spawn = |exe: &Path| {
            let mut command = tokio::process::Command::new(exe);
            command
                .args(&args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            unsafe {
                command.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
                    Ok(())
                });
            }
            command.spawn()
        };
        let child = match spawn(&program) {
            Ok(child) => child,
            Err(_) => match spawn(&cupsd) {
                Ok(child) => child,
                Err(e) => {
                    log::warn!(target: "printing", "No print queue: the scheduler did not start: {e}");
                    return false;
                }
            },
        };
        self.process = Some(child);
        for _ in 0..100 {
            let exited = self
                .process
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
            if self.socket.exists() || exited {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !self.socket.exists() {
            log::warn!(
                target: "printing",
                "The print queue did not come up; its log is {}",
                self.root.join("error.log").display()
            );
            self.stop().await;
            return false;
        }
        log::info!(
            target: "printing",
            "Print queue Selkies listening on {}, spooling to {}, on {} paper",
            self.socket.display(), self.spool.display(), self.paper
        );
        true
    }

    /// End the scheduler: `SIGTERM`, then `SIGKILL` after five seconds.
    pub async fn stop(&mut self) {
        let Some(mut child) = self.process.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Names, the spool's place, the paper, pending documents, the watcher, the scheduler's
    //! discovery, and the state root's claim.

    use super::*;
    use parking_lot::Mutex;

    fn scratch() -> PathBuf {
        super::super::testing::scratch("selkies-printing")
    }

    #[test]
    fn document_names() {
        assert_eq!(document_name("Report (2).pdf"), Some("Report (2).pdf"));
        assert_eq!(
            document_name("scan.PDF"),
            Some("scan.PDF"),
            "case of the suffix does not matter"
        );
        for bad in [
            "../x.pdf",
            "sub/x.pdf",
            ".hidden.pdf",
            ".3.part",
            "notes.txt",
            "x\0.pdf",
            "",
        ] {
            assert_eq!(document_name(bad), None, "{bad:?} is not a document");
        }
    }

    #[test]
    fn the_spool_lives_under_the_state_directory() {
        let root = scratch();
        let state = root.to_str().unwrap();
        assert_eq!(
            spool_path_in("", Some(state), "/home/u"),
            root.join("selkies/print")
        );
        assert_eq!(
            spool_path_in("", None, "/home/u"),
            PathBuf::from("/home/u/.local/state/selkies/print")
        );
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            spool_path_in("~/prints", None, &home),
            PathBuf::from(format!("{home}/prints"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn defaults(text: &str) -> Vec<String> {
        regex::Regex::new(
            r"(?m)^\*Default(?:PageSize|PageRegion|ImageableArea|PaperDimension): (\S+)",
        )
        .unwrap()
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
    }

    #[test]
    fn the_paper_follows_an_explicit_papersize_alone() {
        assert_eq!(
            defaults(PPD),
            vec!["A4"; 4],
            "the shipped queue defaults to A4"
        );
        for requested in ["b5", "letterish"] {
            assert_eq!(default_paper(PPD, requested), (PPD.to_string(), None));
        }
        let (text, size) = default_paper(PPD, "LETTER");
        assert_eq!(size.as_deref(), Some("Letter"));
        assert_eq!(defaults(&text), vec!["Letter"; 4]);
    }

    #[test]
    fn pending_lists_documents_oldest_first() {
        let spool = scratch();
        for (name, delay) in [
            ("second.pdf", 2),
            ("first.pdf", 0),
            ("skip.txt", 0),
            (".part", 0),
        ] {
            let path = spool.join(name);
            std::fs::write(&path, b"%PDF%PDF%PDF").unwrap();
            let when = std::time::SystemTime::now() + Duration::from_secs(delay);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        assert_eq!(
            pending(&spool),
            vec![
                ("first.pdf".to_string(), 12),
                ("second.pdf".to_string(), 12)
            ]
        );
        assert_eq!(
            pending(&spool.join("none")),
            vec![],
            "a missing spool is empty"
        );
        std::fs::remove_dir_all(&spool).unwrap();
    }

    #[tokio::test]
    async fn the_watcher_announces_whole_documents_once() {
        let root = scratch();
        let (spool, elsewhere) = (root.join("spool"), root.join("elsewhere"));
        std::fs::create_dir_all(&elsewhere).unwrap();
        let seen: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut watcher = SpoolWatcher::start(
            spool.clone(),
            Arc::new(move |name: &str, size| sink.lock().push((name.into(), size))),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let settled = |count: usize| {
            let seen = seen.clone();
            async move {
                for _ in 0..60 {
                    if seen.lock().len() >= count {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                tokio::time::sleep(Duration::from_secs_f64(SETTLE_SECONDS + 0.3)).await;
                seen.lock().clone()
            }
        };

        std::fs::write(spool.join(".7.part"), vec![b'x'; 400]).unwrap();
        std::fs::rename(spool.join(".7.part"), spool.join("renamed.pdf")).unwrap();
        assert_eq!(
            settled(1).await,
            vec![("renamed.pdf".to_string(), 400)],
            "renamed into place: once, whole"
        );

        seen.lock().clear();
        {
            use std::io::Write;
            let mut f = std::fs::File::create(spool.join("written.pdf")).unwrap();
            f.write_all(b"%PDF").unwrap();
            f.flush().unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            f.write_all(&vec![b'x'; 1000]).unwrap();
        }
        assert_eq!(
            settled(1).await,
            vec![("written.pdf".to_string(), 1004)],
            "written in place: once, final size"
        );

        seen.lock().clear();
        std::fs::write(elsewhere.join("moved.pdf"), vec![b'x'; 1200]).unwrap();
        std::fs::rename(elsewhere.join("moved.pdf"), spool.join("moved.pdf")).unwrap();
        assert_eq!(
            settled(1).await,
            vec![("moved.pdf".to_string(), 1200)],
            "moved in from another directory"
        );

        seen.lock().clear();
        std::fs::write(spool.join("notes.txt"), b"text").unwrap();
        std::fs::write(spool.join(".hidden.pdf"), b"%PDF").unwrap();
        assert_eq!(
            settled(1).await,
            vec![],
            "other files and hidden ones are not announced"
        );
        watcher.stop();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_locked_scheduler_is_named_with_its_mode() {
        let root = scratch();
        let prefix = root.join("usr");
        let cupsd = prefix.join("sbin/cupsd");
        std::fs::create_dir_all(cupsd.parent().unwrap()).unwrap();
        std::fs::write(&cupsd, b"").unwrap();
        std::fs::set_permissions(&cupsd, std::fs::Permissions::from_mode(0o000)).unwrap();
        let path = cupsd.parent().unwrap().to_str().unwrap().to_string();
        assert_eq!(
            PrintQueue::programs_on(&path),
            None,
            "a scheduler this user cannot read serves no queue"
        );
        assert_eq!(
            PrintQueue::locked_scheduler_on(&path),
            Some((cupsd.clone(), 0)),
            "and is named with its mode"
        );
        std::fs::set_permissions(&cupsd, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(prefix.join("lib/cups/daemon")).unwrap();
        std::fs::create_dir_all(prefix.join("lib/cups/filter")).unwrap();
        std::fs::write(prefix.join("lib/cups/daemon/cups-exec"), b"").unwrap();
        assert_eq!(
            PrintQueue::programs_on(&path),
            Some((
                cupsd.clone(),
                prefix.join("lib/cups"),
                prefix.join("share/cups")
            )),
            "a readable one is found with its program and data directories"
        );
        assert_eq!(
            PrintQueue::locked_scheduler_on(&path),
            None,
            "and nothing stands in its way"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_state_root_is_this_users_alone() {
        let tmp = scratch();
        let root = PrintQueue::state_root(None, &tmp);
        assert_eq!(
            root.parent(),
            Some(tmp.as_path()),
            "the state root falls back to the temporary directory"
        );
        let queue = PrintQueue::with_root(tmp.join("spool"), root.clone());
        assert!(
            queue.claim_root().is_ok(),
            "an absent root is created for this user alone"
        );
        assert_eq!(
            std::fs::symlink_metadata(&root).unwrap().mode() & 0o7777,
            0o700
        );
        assert!(
            queue.claim_root().is_ok(),
            "and taken again on the next start"
        );
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            queue.claim_root().is_err(),
            "a root others can write is refused"
        );
        std::fs::remove_dir(&root).unwrap();
        let planted = tmp.join("planted");
        std::fs::create_dir(&planted).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&planted, &root).unwrap();
        assert!(
            queue.claim_root().is_err(),
            "a link planted at its name is refused"
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
