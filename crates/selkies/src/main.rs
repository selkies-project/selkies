/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Process entry point for the Selkies streaming server.
//!
//! `selkies --version` prints the package version and the capture stack it links and exits;
//! everything else is the server, which arrives module by module.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!(
            "selkies {} ({})",
            env!("CARGO_PKG_VERSION"),
            selkies::capture_stack()
        );
        return ExitCode::SUCCESS;
    }
    eprintln!("selkies: the Rust server is not serving yet; see NOTES.md");
    ExitCode::FAILURE
}
