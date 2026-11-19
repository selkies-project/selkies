//! A private session bus for the tests of the shell's D-Bus servers: a
//! `dbus-daemon` of the test's own, ended with it, so a test never meets
//! the desktop's own servers.

use std::io::BufRead;
use std::process::{Child, Command, Stdio};

use zbus::connection::Builder;

/// A private session bus, ended with the test.
pub struct Bus {
    daemon: Child,
    address: String,
}

impl Bus {
    /// Starts the daemon, or `None` where there is none to start.
    pub fn start() -> Option<Bus> {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let mut address = String::new();
        std::io::BufReader::new(daemon.stdout.take()?)
            .read_line(&mut address)
            .ok()?;
        Some(Bus {
            daemon,
            address: address.trim().to_string(),
        })
    }

    /// A builder for a connection to the bus.
    pub fn builder(&self) -> Builder<'static> {
        Builder::address(self.address.as_str())
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}
