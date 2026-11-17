// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Busy processes that share one CPU with a server's async runtime, so the sender it runs falls
//! behind its encoder in bursts.
//!
//! The runtime's threads ([`threads`], read before a page connects) are pinned beside the busy
//! processes while a burst runs and go back to every CPU they had between bursts. Each busy
//! process runs in a session of its own: the kernel shares a CPU between sessions before it
//! shares it between their threads (autogroup), so busy processes in one session leave the
//! server half of it however many there are, and a sender mostly keeps up on half a CPU. Sixteen
//! leave the runtime about a seventeenth: 80 ms bursts let frames go in about half of them,
//! against one in eight with three busy processes in one session and one in four with eight in
//! sessions of their own. A stalling load also sets the threads to the lowest priority, which
//! counts where the kernel groups no sessions. Raising a thread's niceness needs no privilege
//! and lowering it back does, so stalled threads keep that priority to the end.

use std::process::{Child, Command, Stdio};

/// Busy processes sharing the runtime's CPU.
const HOGS: usize = 16;

/// The threads `pid` runs now. Read before a page connects, they are its async runtime's
/// workers, which send the frames, and the helpers started with them: a capture and its encoder
/// start on threads of their own once a page asks for video, so a load on these slows what sends
/// the frames and not what makes them. The runtime's workers carry no name a capture's threads
/// lack (a thread takes its creator's), so they are told apart by having come first.
pub fn threads(pid: u32) -> Vec<i32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut tids: Vec<i32> = tasks
        .filter_map(|t| t.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    tids.sort_unstable();
    tids
}

/// A CPU set of `cpus`.
fn cpu_set(cpus: &[usize]) -> libc::cpu_set_t {
    // SAFETY: cpu_set_t is a plain bit array, valid all zero.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &cpu in cpus {
        // SAFETY: CPU_SET writes one bit of `set`, and `cpu` is below CPU_SETSIZE.
        unsafe { libc::CPU_SET(cpu, &mut set) };
    }
    set
}

/// The CPUs `set` holds.
fn cpus_of(set: &libc::cpu_set_t) -> Vec<usize> {
    let size = usize::try_from(libc::CPU_SETSIZE).unwrap_or(1024);
    // SAFETY: CPU_ISSET reads one bit of `set`, and every index is below CPU_SETSIZE.
    (0..size)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, set) })
        .collect()
}

/// Pin `tid` to `set`; a thread gone since is passed over.
fn pin(tid: i32, set: &libc::cpu_set_t) {
    // SAFETY: the kernel reads `size_of::<cpu_set_t>()` bytes of `set`, which it holds.
    unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), set) };
}

/// Send `signal` to `child`.
fn signal(child: &Child, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill reads no memory; a pid gone since fails harmlessly.
        unsafe { libc::kill(pid, signal) };
    }
}

/// The load, stopped between bursts; dropping it ends the busy processes and unpins the threads.
pub struct Load {
    /// Each runtime thread and the CPUs it had.
    original: Vec<(i32, libc::cpu_set_t)>,
    cpu: libc::cpu_set_t,
    hogs: Vec<Child>,
}

impl Load {
    /// Start the busy processes on the threads' highest CPU and stop them until [`Load::resume`];
    /// `stall` also sets the threads to the lowest priority.
    pub fn start(threads: &[i32], stall: bool) -> Load {
        let mut original = Vec::new();
        for &tid in threads {
            let mut set = cpu_set(&[]);
            // SAFETY: the kernel writes at most `size_of::<cpu_set_t>()` bytes into `set`.
            let got = unsafe {
                libc::sched_getaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &mut set)
            };
            if got == 0 {
                original.push((tid, set));
            }
        }
        let highest = original
            .iter()
            .flat_map(|(_, set)| cpus_of(set))
            .max()
            .unwrap_or(0);
        let cpu = cpu_set(&[highest]);
        if stall {
            for &(tid, _) in &original {
                if let Ok(who) = libc::id_t::try_from(tid) {
                    // SAFETY: setpriority reads no memory; a thread gone since fails harmlessly.
                    unsafe { libc::setpriority(libc::PRIO_PROCESS, who, 19) };
                }
            }
        }
        let mut hogs = Vec::new();
        for _ in 0..HOGS {
            let mut command = Command::new("sh");
            command
                .args(["-c", "while :; do :; done"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::server::tie_to_this_thread(&mut command);
            let hog = command.spawn().expect("a busy process");
            if let Ok(pid) = i32::try_from(hog.id()) {
                pin(pid, &cpu);
            }
            hogs.push(hog);
        }
        let load = Load {
            original,
            cpu,
            hogs,
        };
        load.pause();
        load
    }

    /// Stop the busy processes and give the threads back every CPU they had.
    pub fn pause(&self) {
        for hog in &self.hogs {
            signal(hog, libc::SIGSTOP);
        }
        for (tid, set) in &self.original {
            pin(*tid, set);
        }
    }

    /// Pin the threads beside the busy processes and let those run.
    pub fn resume(&self) {
        for (tid, _) in &self.original {
            pin(*tid, &self.cpu);
        }
        for hog in &self.hogs {
            signal(hog, libc::SIGCONT);
        }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        for hog in &mut self.hogs {
            signal(hog, libc::SIGCONT);
            let _ = hog.kill();
        }
        for hog in &mut self.hogs {
            let _ = hog.wait();
        }
        for (tid, set) in &self.original {
            pin(*tid, set);
        }
    }
}
