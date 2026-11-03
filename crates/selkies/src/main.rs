/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Process entry point for the Selkies streaming server.
//!
//! Resolves the settings, configures logging, publishes the socket directories the
//! interposers read, settles the desktop's density on X11, probes the encoders this host
//! serves once, and runs the stream server on a tokio runtime until a signal unwinds it.
//! `selkies --version` prints the package version and the capture stack it links and exits,
//! `selkies --help` the settings, and `selkies <command>` runs one of the tools `commands`
//! lists instead.

use std::process::ExitCode;

use log::{error, info, warn};
use selkies::settings::{self, Settings};

/// `selkies gpu-probe`, over the settings the probe reads.
fn gpu_probe_tool(_prog: &str, args: &[String]) -> i32 {
    settings::install(Settings::parse(&[], &|name| std::env::var(name).ok()));
    selkies::stats::gpu_probe::main(args)
}

/// `selkies resize WxH`: resizes the display and prints the size it took as `(w, h)`, or `None`;
/// `selkies session` reads that line.
fn resize_tool(prog: &str, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: {prog} <WxH>");
        return 0;
    }
    let Some(resolution) = args.first() else {
        eprintln!("usage: {prog} <WxH>");
        return 2;
    };
    settings::install(Settings::parse(&[], &|name| std::env::var(name).ok()));
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return 1;
    };
    match runtime.block_on(selkies::display::resize_display(resolution, None, None)) {
        Some((w, h)) => println!("({w}, {h})"),
        None => println!("None"),
    }
    0
}

/// `selkies manual-size [server arguments]`: the `WxH` a manual resolution locks the stream to,
/// resolved from the arguments and environment the server would be started with, or nothing
/// where none is set; the image's entrypoint presets the X display to it before the server starts.
fn manual_size_tool(_prog: &str, args: &[String]) -> i32 {
    let resolved = Settings::parse(args, &|name| std::env::var(name).ok());
    if resolved.manual_resolution().on {
        println!("{}x{}", resolved.manual_width(), resolved.manual_height());
    }
    0
}

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
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", Settings::help_text(&selkies::commands::help_epilog()));
        return ExitCode::SUCCESS;
    }
    selkies::commands::install_tool("gpu-probe", gpu_probe_tool);
    selkies::commands::install_tool("resize", resize_tool);
    selkies::commands::install_tool("manual-size", manual_size_tool);
    if let Some(status) = selkies::commands::run(&std::env::args().collect::<Vec<_>>()) {
        return ExitCode::from(status.clamp(0, 255) as u8);
    }
    let resolved = Settings::parse(&args, &|name| std::env::var(name).ok());
    selkies::log::configure(resolved.debug().on);
    for token in &resolved.unknown_args {
        if token.starts_with('-') {
            warn!(target: "settings", "Ignoring unrecognized argument {}", token.split('=').next().unwrap_or(token));
        }
    }
    settings::install(resolved);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(target: "main", "Error in main: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(selkies::server::run()) {
        Ok(()) => {
            info!(target: "main", "Server stopped by signal");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(target: "main", "Error in main: {e}");
            ExitCode::FAILURE
        }
    }
}
