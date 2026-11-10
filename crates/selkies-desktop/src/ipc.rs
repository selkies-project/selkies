//! The control socket: a second invocation of the binary sends a one-line
//! command to the running desktop, which is how a compositor keybinding opens
//! the menu.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

/// A command another process sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open or close the start menu.
    ToggleMenu,
    /// Open or close the quick launcher.
    Launcher,
    /// Read the applications and settings again.
    Reload,
    /// Minimize every window, or restore them.
    ShowDesktop,
    /// Open a file manager window, on a folder when one is given.
    Files(Option<PathBuf>),
    /// Open the settings window, on a page by id when one is given.
    Settings(Option<String>),
    /// Ask for a name and make a folder on the desktop, on the cell last pointed at.
    NewFolder,
    /// A window reaches the bar's edge, or none does any more: the window manager's word on
    /// whether a floating bar attaches to the edge.
    BarAttached(bool),
    /// Exit the desktop.
    Quit,
}

impl Command {
    /// The command for a line, as typed on the command line or sent over the
    /// socket: a word, followed for `files` by an optional path and for
    /// `settings` by an optional page id.
    ///
    /// # Arguments
    /// * `line` - The command, with or without leading dashes.
    pub fn parse(line: &str) -> Option<Command> {
        let line = line.trim();
        let (word, rest) = match line.split_once(char::is_whitespace) {
            Some((word, rest)) => (word, rest.trim()),
            None => (line, ""),
        };
        match word.trim_start_matches('-') {
            "toggle-menu" | "menu" => Some(Command::ToggleMenu),
            "launcher" => Some(Command::Launcher),
            "reload" => Some(Command::Reload),
            "show-desktop" => Some(Command::ShowDesktop),
            "files" => Some(Command::Files((!rest.is_empty()).then(|| PathBuf::from(rest)))),
            "settings" => Some(Command::Settings((!rest.is_empty()).then(|| rest.to_string()))),
            "new-folder" => Some(Command::NewFolder),
            "bar-attached" => Some(Command::BarAttached(rest == "1")),
            "quit" | "exit" => Some(Command::Quit),
            _ => None,
        }
    }

    /// The line sent over the socket.
    pub fn line(&self) -> String {
        match self {
            Command::ToggleMenu => "toggle-menu".to_string(),
            Command::Launcher => "launcher".to_string(),
            Command::Reload => "reload".to_string(),
            Command::ShowDesktop => "show-desktop".to_string(),
            Command::Files(None) => "files".to_string(),
            Command::Files(Some(path)) => format!("files {}", path.display()),
            Command::Settings(None) => "settings".to_string(),
            Command::Settings(Some(page)) => format!("settings {page}"),
            Command::NewFolder => "new-folder".to_string(),
            Command::BarAttached(attached) => format!("bar-attached {}", u8::from(*attached)),
            Command::Quit => "quit".to_string(),
        }
    }
}

/// The socket path: under the runtime directory, named after the display so
/// one desktop per session is reachable.
pub fn socket_path() -> PathBuf {
    selkies_draw::style::desktop_socket_path(|name| std::env::var(name).ok())
}

/// The listening side, owned by the running desktop.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
}

impl Server {
    /// Binds the control socket, replacing a stale one left by an earlier run.
    ///
    /// # Returns
    /// The server, or `None` when the socket cannot be created.
    pub fn bind() -> Option<Server> {
        let path = socket_path();
        if path.exists() && UnixStream::connect(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path).ok()?;
        listener.set_nonblocking(true).ok()?;
        Some(Server { listener, path })
    }

    /// The descriptor to poll for readability.
    pub fn fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Accepts every waiting connection and reads its command.
    pub fn drain(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        while let Ok((mut stream, _)) = self.listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut text = String::new();
            if stream.read_to_string(&mut text).is_ok() || !text.is_empty() {
                for line in text.lines() {
                    if let Some(command) = Command::parse(line) {
                        commands.push(command);
                    }
                }
            }
        }
        commands
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Sends a command to the running desktop.
///
/// # Arguments
/// * `command` - The command.
///
/// # Errors
/// No desktop is listening, or the write failed.
pub fn send(command: Command) -> Result<(), String> {
    let path = socket_path();
    let mut stream =
        UnixStream::connect(&path).map_err(|e| format!("no running desktop at {}: {e}", path.display()))?;
    stream
        .write_all(format!("{}\n", command.line()).as_bytes())
        .map_err(|e| format!("{e}"))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(Command::parse("--toggle-menu"), Some(Command::ToggleMenu));
        assert_eq!(Command::parse("reload\n"), Some(Command::Reload));
        assert_eq!(Command::parse("--launcher"), Some(Command::Launcher));
        assert_eq!(Command::Launcher.line(), "launcher");
        assert_eq!(Command::parse("show-desktop"), Some(Command::ShowDesktop));
        assert_eq!(Command::parse("files"), Some(Command::Files(None)));
        assert_eq!(Command::parse("--settings"), Some(Command::Settings(None)));
        assert_eq!(
            Command::parse("--settings wallpaper"),
            Some(Command::Settings(Some("wallpaper".to_string())))
        );
        assert_eq!(
            Command::Settings(Some("about".to_string())).line(),
            "settings about"
        );
        assert_eq!(
            Command::parse("--files /tmp/my dir"),
            Some(Command::Files(Some(PathBuf::from("/tmp/my dir"))))
        );
        assert_eq!(Command::parse("dance"), None);
    }

    #[test]
    fn round_trips_over_socket() {
        let dir = std::env::temp_dir().join(format!("selkies-desktop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
            std::env::set_var("WAYLAND_DISPLAY", "test-0");
        }
        let server = Server::bind().expect("bind");
        send(Command::ShowDesktop).expect("send");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(server.drain(), vec![Command::ShowDesktop]);
        drop(server);
        assert!(!socket_path().exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
