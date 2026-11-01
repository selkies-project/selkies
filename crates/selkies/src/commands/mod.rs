/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The tools `selkies <command>` runs besides the server.
//!
//! `selkies <command>` reaches every tool from the one executable, where a packaging carries
//! nothing else: `session` ([`session`]) starts a desktop session and Selkies streaming it,
//! `resize` sets the X display's size, and `gpu-probe` reports the GPU a session renders and
//! encodes on. The command sees the arguments after its name, under the name it was called
//! by (`selkies resize`), as a console script of its own would. `session` lives here; the two
//! display tools are the display and stats modules', which hand them in through
//! [`install_tool`] at startup, so a build that lacks one says so rather than guessing.
//! `selkies --help` lists the commands after the server's options ([`help_epilog`]).

pub mod session;
pub mod sessions;

use std::collections::HashMap;
use std::sync::Mutex;

/// One command: its name and what it does, as `selkies --help` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub summary: &'static str,
}

/// The commands, in the order `--help` lists them.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "session",
        summary: "start a desktop session and Selkies streaming it",
    },
    Command {
        name: "resize",
        summary: "resize the X display to WxH",
    },
    Command {
        name: "gpu-probe",
        summary: "report the GPU a session renders and encodes on",
    },
];

/// A tool another module implements: `prog` is the name it was called by (`selkies resize`),
/// `args` the arguments after the command's name; the result is its exit status.
pub type ToolFn = fn(prog: &str, args: &[String]) -> i32;

static TOOLS: Mutex<Option<HashMap<&'static str, ToolFn>>> = Mutex::new(None);

/// Hands in the implementation of `name`, one of [`COMMANDS`] other than `session`; false
/// for a name `--help` does not list.
pub fn install_tool(name: &'static str, tool: ToolFn) -> bool {
    if name == "session" || !COMMANDS.iter().any(|c| c.name == name) {
        return false;
    }
    TOOLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(name, tool);
    true
}

fn tool(name: &str) -> Option<ToolFn> {
    TOOLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|tools| tools.get(name).copied())
}

/// The commands, as the server's `--help` lists them after its own options.
pub fn help_epilog() -> String {
    let width = COMMANDS.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let lines: Vec<String> = COMMANDS
        .iter()
        .map(|c| format!("  selkies {:<width$}  {}", c.name, c.summary))
        .collect();
    format!("commands:\n{}", lines.join("\n"))
}

/// Run the command `argv[1]` names and return its exit status, or `None` when it names none,
/// leaving `argv` to the server.
pub fn run(argv: &[String]) -> Option<i32> {
    let name = argv.get(1)?;
    let command = COMMANDS.iter().find(|c| c.name == name)?;
    let prog = format!("selkies {}", command.name);
    let args = &argv[2..];
    if command.name == "session" {
        return Some(session::main(args, &prog));
    }
    match tool(command.name) {
        Some(tool) => Some(tool(&prog, args)),
        None => {
            eprintln!("{prog}: this build carries no such tool");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn help_lists_every_command() {
        let epilog = help_epilog();
        assert!(epilog.starts_with("commands:\n"));
        for command in COMMANDS {
            let line = epilog
                .lines()
                .find(|l| l.starts_with(&format!("  selkies {} ", command.name)))
                .unwrap_or_else(|| panic!("{} is listed", command.name));
            assert!(line.ends_with(command.summary));
        }
        assert_eq!(epilog.lines().count(), COMMANDS.len() + 1);
    }

    #[test]
    fn no_command_leaves_argv_to_the_server() {
        assert_eq!(run(&argv(&["selkies"])), None);
        assert_eq!(run(&argv(&["selkies", "--port=1"])), None);
        assert_eq!(run(&argv(&["selkies", "resize-me"])), None);
    }

    #[test]
    fn session_takes_its_own_arguments_under_its_own_name() {
        assert_eq!(run(&argv(&["selkies", "session", "--help"])), Some(0));
        assert_eq!(run(&argv(&["selkies", "session", "--session"])), Some(2));
    }

    #[test]
    fn tools_are_handed_in_by_name() {
        assert!(!install_tool("session", |_, _| 0));
        assert!(!install_tool("bogus", |_, _| 0));
    }
}
