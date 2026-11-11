//! The volume control's bridge to PulseAudio, through `pactl`: a `pactl
//! subscribe` child reports sink changes, and the level is read and set with
//! one short `pactl` invocation each.

use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::desktop::Volume;

/// How often the level is polled when no subscription is running.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How often a missing PulseAudio is probed again.
const RETRY_INTERVAL: Duration = Duration::from_secs(20);

/// The audio bridge.
pub struct Audio {
    subscriber: Option<Child>,
    reader: Option<BufReader<std::process::ChildStdout>>,
    last_poll: Instant,
    available: bool,
    pending_refresh: bool,
}

impl Audio {
    /// Starts the bridge; nothing runs when `pactl` is not installed.
    pub fn new() -> Audio {
        let mut audio = Audio {
            subscriber: None,
            reader: None,
            last_poll: Instant::now() - POLL_INTERVAL,
            available: false,
            pending_refresh: true,
        };
        audio.start_subscriber();
        audio
    }

    /// Spawns `pactl subscribe` so sink changes wake the main loop.
    fn start_subscriber(&mut self) {
        if self.subscriber.is_some() {
            return;
        }
        let child = Command::new("pactl")
            .arg("subscribe")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child
            && let Some(stdout) = child.stdout.take()
        {
            let fd = stdout.as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            self.reader = Some(BufReader::new(stdout));
            self.subscriber = Some(child);
        }
    }

    /// The descriptor to poll for readability, when a subscription is running.
    pub fn fd(&self) -> Option<RawFd> {
        self.reader.as_ref().map(|r| r.get_ref().as_raw_fd())
    }

    /// Reads the subscription output; a sink or server event schedules a
    /// refresh of the level.
    pub fn drain(&mut self) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    self.reader = None;
                    if let Some(mut child) = self.subscriber.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    self.pending_refresh = true;
                    return;
                }
                Ok(_) => {
                    if line.contains("sink") || line.contains("server") {
                        self.pending_refresh = true;
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// Reads the level when a refresh is due.
    ///
    /// # Arguments
    /// * `now` - The current time.
    ///
    /// # Returns
    /// The new volume reading when one was taken, `None` otherwise.  The inner
    /// `None` means PulseAudio is unavailable.
    pub fn poll(&mut self, now: Instant) -> Option<Option<Volume>> {
        let interval = if self.available && self.subscriber.is_some() {
            POLL_INTERVAL * 6
        } else if self.available {
            POLL_INTERVAL
        } else {
            RETRY_INTERVAL
        };
        if !self.pending_refresh && now.duration_since(self.last_poll) < interval {
            return None;
        }
        self.pending_refresh = false;
        self.last_poll = now;
        let volume = read_volume();
        self.available = volume.is_some();
        if self.available && self.subscriber.is_none() {
            self.start_subscriber();
        }
        Some(volume)
    }

    /// When `poll` next has something to do.
    pub fn deadline(&self, now: Instant) -> Instant {
        if self.pending_refresh {
            return now;
        }
        let interval = if self.available && self.subscriber.is_some() {
            POLL_INTERVAL * 6
        } else if self.available {
            POLL_INTERVAL
        } else {
            RETRY_INTERVAL
        };
        self.last_poll + interval
    }

    /// Sets the default sink's volume.
    ///
    /// # Arguments
    /// * `percent` - The level in percent.
    pub fn set_volume(&mut self, percent: u8) {
        let _ = Command::new("pactl")
            .args(["set-sink-volume", "@DEFAULT_SINK@", &format!("{percent}%")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        self.pending_refresh = true;
    }

    /// Toggles the default sink's mute.
    pub fn toggle_mute(&mut self) {
        let _ = Command::new("pactl")
            .args(["set-sink-mute", "@DEFAULT_SINK@", "toggle"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        self.pending_refresh = true;
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        if let Some(mut child) = self.subscriber.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Runs a `pactl` query and returns its output.
fn pactl(args: &[&str]) -> Option<String> {
    let output = Command::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Reads the default sink's volume and mute state.
fn read_volume() -> Option<Volume> {
    let volume = pactl(&["get-sink-volume", "@DEFAULT_SINK@"])?;
    let percent = parse_percent(&volume)?;
    let mute = pactl(&["get-sink-mute", "@DEFAULT_SINK@"])?;
    Some(Volume {
        percent,
        muted: mute.contains("yes"),
    })
}

/// The first percentage in `pactl get-sink-volume` output.
///
/// # Arguments
/// * `text` - The command output.
pub fn parse_percent(text: &str) -> Option<u8> {
    text.split_whitespace()
        .find_map(|word| word.strip_suffix('%').and_then(|n| n.parse::<u32>().ok()))
        .map(|p| p.min(255) as u8)
}

#[cfg(test)]
mod tests {
    use super::parse_percent;

    #[test]
    fn parses_pactl_volume() {
        let text = "Volume: front-left: 39321 /  60% / -13.31 dB,   front-right: 39321 /  60% / -13.31 dB\n        balance 0.00";
        assert_eq!(parse_percent(text), Some(60));
        assert_eq!(parse_percent("Volume: mono: 98304 / 150% / 10.57 dB"), Some(150));
        assert_eq!(parse_percent("nothing here"), None);
    }
}
