/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A desktop agent that daemonizes as ssh-agent does, for the session launcher's suite.
//!
//! `session-agent PIDFILE` forks, leaves its parent to exit at once, starts a session of its
//! own, makes itself undumpable so its environment cannot be read from /proc, writes its pid to
//! PIDFILE (through a rename, so a reader never sees half of it), and sleeps for an hour. What
//! ends it is whoever ends the session.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(pidfile) = std::env::args().nth(1) else {
        eprintln!("usage: session-agent PIDFILE");
        return ExitCode::from(2);
    };
    // SAFETY: called before any thread is started; the child goes on with only this thread.
    let child = unsafe { libc::fork() };
    if child < 0 {
        eprintln!("session-agent: fork: {}", std::io::Error::last_os_error());
        return ExitCode::FAILURE;
    }
    if child > 0 {
        return ExitCode::SUCCESS;
    }
    // SAFETY: plain system calls on this process.
    unsafe {
        libc::setsid();
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    let staged = format!("{pidfile}.tmp");
    if std::fs::write(&staged, std::process::id().to_string()).is_err()
        || std::fs::rename(&staged, &pidfile).is_err()
    {
        return ExitCode::FAILURE;
    }
    std::thread::sleep(std::time::Duration::from_secs(3600));
    ExitCode::SUCCESS
}
