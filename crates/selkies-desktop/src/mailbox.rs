//! A channel from the shell's D-Bus servers to its loop: values go through a
//! standard channel and a byte on a pipe wakes the loop, whose `poll` watches
//! the pipe's read end beside the display's and the other descriptors.
//!
//! The servers run on zbus's executor threads and on threads of their own, so
//! the sending half is `Send` and `Sync` and never blocks: the pipe is
//! non-blocking, and a write that would block is dropped, since a full pipe
//! already wakes the loop.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{Receiver, Sender, channel as std_channel};

/// The sending half, held by the server.
pub struct Post<T> {
    /// The channel to the loop.
    sender: Sender<T>,
    /// The write end of the pipe that wakes the loop.
    wake: File,
}

impl<T> Post<T> {
    /// Hands a value to the loop and wakes it.  A loop that went away drops
    /// the value.
    pub fn send(&self, value: T) {
        if self.sender.send(value).is_ok() {
            let _ = (&self.wake).write(&[1]);
        }
    }
}

/// The receiving half, held by the loop.
pub struct Mailbox<T> {
    /// What the servers handed over.
    receiver: Receiver<T>,
    /// The read end of the pipe that wakes the loop.
    wake: File,
}

impl<T> Mailbox<T> {
    /// The descriptor that becomes readable when something arrived.
    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Takes everything that arrived, clearing the wake pipe.
    pub fn drain(&self) -> Vec<T> {
        let mut buf = [0u8; 64];
        while (&self.wake).read(&mut buf).is_ok_and(|n| n > 0) {}
        self.receiver.try_iter().collect()
    }
}

/// A connected pair.
///
/// # Errors
/// The pipe could not be made.
pub fn channel<T>() -> std::io::Result<(Post<T>, Mailbox<T>)> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded, so both descriptors are open and owned by nothing else.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let (sender, receiver) = std_channel();
    Ok((
        Post {
            sender,
            wake: File::from(write),
        },
        Mailbox {
            receiver,
            wake: File::from(read),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_sent_wake_the_pipe_and_drain_in_order() {
        let (post, mailbox) = channel().unwrap();
        post.send(1);
        post.send(2);
        let mut fds = [libc::pollfd {
            fd: mailbox.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        assert_eq!(unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) }, 1);
        assert_eq!(mailbox.drain(), [1, 2]);
        assert_eq!(
            unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) },
            0,
            "draining clears the pipe"
        );
    }
}
