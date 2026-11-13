/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The apps panel is published only where its runner can work.
//!
//! Its buttons are `selkies-proot` calls over the command channel, so a session whose runner is
//! missing or refuses the environment (proot cannot start an app there) can offer nothing that
//! succeeds; the server publishes `ui_sidebar_show_apps` as that effective availability, which is
//! what both dashboards gate the panel on. A disabled command channel is permission, not
//! availability: the panel still publishes and the client explains why its actions are off.

use std::path::Path;

use selkies_suites::{Checks, Client, path_with, script, server, workdir};
use serde_json::Value;

/// Write a `selkies-proot` stand-in in `dir` whose `check` exits with `status`.
fn runner_stub(dir: &Path, status: u8) {
    script(
        &dir.join("selkies-proot"),
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = check ]; then\n\
             \x20 [ {status} -eq 0 ] || echo \"selkies-proot: proot cannot start an app here\" >&2\n\
             \x20 exit {status}\n\
             fi\n\
             exit 0\n"
        ),
    );
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_apps_panel_is_published_only_where_its_runner_can_work() {
    let mut checks = Checks::new("apps-gate");
    let works = workdir().join("apps-ok-bin");
    let refuses = workdir().join("apps-bad-bin");
    let empty = workdir().join("apps-none-bin");
    runner_stub(&works, 0);
    runner_stub(&refuses, 1);
    std::fs::create_dir_all(&empty).expect("an empty directory");
    let mut logs = String::new();
    for (label, shown, commands, path) in [
        (
            "runner reports it can run here",
            true,
            "true",
            path_with(&works),
        ),
        (
            "runner reports the environment denies it",
            false,
            "true",
            path_with(&refuses),
        ),
        (
            "no runner installed",
            false,
            "true",
            empty.display().to_string(),
        ),
        // Permission is not availability: with commands disabled the panel still publishes and
        // the client explains why its actions are off.
        (
            "command channel disabled but runner present",
            true,
            "false",
            path_with(&works),
        ),
    ] {
        let server = server::launch("gate")
            .env("SELKIES_COMMAND_ENABLED", commands)
            .env("PATH", &path)
            .start()
            .await;
        let page = Client::connect(&server, "").await;
        let published = page.published_setting("ui_sidebar_show_apps").await;
        checks.check(
            &format!(
                "apps panel {}: {label}",
                if shown { "shown" } else { "hidden" }
            ),
            published == Some(Value::Bool(shown)),
            format!("ui_sidebar_show_apps={published:?}"),
        );
        logs.push_str(&server.stop().await);
    }
    checks.finish(&logs);
}
