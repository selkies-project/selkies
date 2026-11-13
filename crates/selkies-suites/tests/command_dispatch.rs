/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Global and command-local help reach their respective handlers.

use std::process::Command;
use std::time::Duration;

use selkies_suites::{Checks, server_binary};

/// What the server's own help opens with, which a command's help must not print.
const SERVER_HELP: &str = "Selkies WebSocket Streaming Server";

/// The output of the server run with `args`, both streams, when it exits zero within its bound.
fn help(args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(server_binary())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{} did not start: {e}", server_binary().display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("still running after 30s".to_owned());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!("{}: {text}", output.status))
    }
}

#[test]
#[ignore = "needs a built selkies server (SELKIES_TEST_SERVER)"]
fn each_help_flag_reaches_the_handler_of_its_own_command() {
    let mut checks = Checks::new("commands");
    for (label, args, wanted, unwanted) in [
        ("global", &["--help"][..], SERVER_HELP, None),
        (
            "session",
            &["session", "--help"][..],
            "Start a desktop session and Selkies streaming it",
            Some(SERVER_HELP),
        ),
        (
            "resize",
            &["resize", "--help"][..],
            "usage: selkies resize <WxH>",
            Some(SERVER_HELP),
        ),
    ] {
        let output = help(args);
        let ok = output.as_ref().is_ok_and(|text| {
            text.contains(wanted) && unwanted.is_none_or(|other| !text.contains(other))
        });
        let shown = match &output {
            Ok(text) => text.lines().next().unwrap_or_default().to_owned(),
            Err(why) => why.clone(),
        };
        checks.check(&format!("{label} help uses the local handler"), ok, shown);
    }
    checks.finish("");
}
