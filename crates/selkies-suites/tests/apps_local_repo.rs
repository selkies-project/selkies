/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A session on a proot-apps local repository lists that repository.
//!
//! With `PA_REPO_FOLDER` set, proot-apps installs only what that folder holds, so the server
//! serves the folder's catalog under `/api/apps/` and tells the page to read it there
//! (`apps_local_repo` in the settings payload) instead of the remote one. The folder reaches the
//! server through its own setting as well as through the runner's variable, and the server hands
//! it to the runner either way, so the panel and the runner never read two repositories. Without
//! a repository nothing is served and the page reads the remote catalog.
//!
//! Driven against a stand-in `selkies-proot` that records the environment its `check` ran in,
//! and a repository laid out in the work directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use selkies_suites::{Checks, Client, Server, http, path_with, script, server, workdir};
use serde_json::Value;

const METADATA: &str = "include:\n  - name: localapp\n    full_name: Local App\n    \
                        description: From the local repository\n    icon: localapp.svg\n";
const ICON: &str = "<svg xmlns='http://www.w3.org/2000/svg'/>";

/// What one test of the suite runs on: a stand-in runner, where it records the environment of its
/// `check`, and a repository with one application and its catalog.
struct Fixture {
    path: String,
    recorded: PathBuf,
    repository: PathBuf,
}

impl Fixture {
    /// Lay the fixture out under the work directory, in a folder named after `test`.
    fn new(test: &str) -> Fixture {
        let root = workdir().join(format!("apps-local-repo-{test}"));
        let _ = std::fs::remove_dir_all(&root);
        let recorded = root.join("env");
        let bin = root.join("bin");
        script(
            &bin.join("selkies-proot"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = check ]; then env > \"{}\"; fi\nexit 0\n",
                recorded.display()
            ),
        );
        let repository = root.join("repo");
        let images = repository.join("metadata/img");
        std::fs::create_dir_all(repository.join("ghcr.io_linuxserver_proot-apps_localapp"))
            .expect("the application's folder");
        std::fs::create_dir_all(&images).expect("the catalog's folder");
        std::fs::write(repository.join("metadata/metadata.yml"), METADATA).expect("the catalog");
        std::fs::write(images.join("localapp.svg"), ICON).expect("the icon");
        Fixture {
            path: path_with(&bin),
            recorded,
            repository,
        }
    }

    /// A server with the command channel on and the stand-in runner, and `variable` naming the
    /// repository when one is given.
    async fn server(&self, label: &str, variable: Option<&str>) -> Server {
        let mut launch = server::launch(label)
            .env("PATH", &self.path)
            .env("SELKIES_COMMAND_ENABLED", "true");
        if let Some(variable) = variable {
            launch = launch.env(variable, &self.repository.display().to_string());
        }
        launch.start().await
    }
}

/// The status of a GET on `server`, and the body as text; 0 when the request failed.
async fn get(server: &Server, path: &str) -> (u16, String) {
    match http::get(server.port(), path).await {
        Ok(response) => (response.status, response.text()),
        Err(why) => (0, why),
    }
}

/// What the connect-time settings payload publishes for `apps_local_repo`.
async fn local_repo_published(server: &Server) -> Option<Value> {
    let page = Client::connect(server, "").await;
    let published = page.published_setting("apps_local_repo").await;
    page.close().await;
    published
}

/// The environment the runner's `check` recorded, once it ran: the runner is probed at startup,
/// which the status endpoint does not wait for.
async fn recorded_environment(file: &Path) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let recorded = std::fs::read_to_string(file).unwrap_or_default();
        if recorded.ends_with('\n') || tokio::time::Instant::now() >= deadline {
            return recorded;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn a_repository_named_by_the_selkies_setting_reaches_the_runner_and_is_served() {
    let fixture = Fixture::new("setting");
    let folder = fixture.repository.display().to_string();
    let mut checks = Checks::new("apps-local-repo");
    // The Selkies setting alone, so the server has to hand the folder to the runner.
    let server = fixture
        .server("setting", Some("SELKIES_APPS_REPO_FOLDER"))
        .await;
    let recorded = recorded_environment(&fixture.recorded).await;
    checks.check(
        "the runner is handed the repository as PA_REPO_FOLDER",
        recorded.contains(&format!("PA_REPO_FOLDER={folder}\n")),
        recorded.chars().take(200).collect::<String>(),
    );
    checks.check(
        "the page is told the catalog is local",
        local_repo_published(&server).await == Some(Value::Bool(true)),
        "",
    );
    let (status, body) = get(&server, "/api/apps/metadata.yml").await;
    checks.check(
        "the repository's catalog is served",
        status == 200 && body == METADATA,
        format!("{status} {body:.80}"),
    );
    let (status, body) = get(&server, "/api/apps/img/localapp.svg").await;
    checks.check(
        "its icon is served",
        status == 200 && body == ICON,
        format!("{status} {body:.80}"),
    );
    let (status, _) = get(&server, "/api/apps/img/..%2Fmetadata.yml").await;
    checks.check(
        "a path in place of an icon name is refused",
        status == 404,
        status,
    );
    let (status, _) = get(&server, "/api/apps/img/localapp.svg%00.png").await;
    checks.check("as is a name with a NUL byte in it", status == 404, status);
    let (status, _) = get(&server, "/api/apps/img/absent.svg").await;
    checks.check(
        "an icon the repository lacks is not found",
        status == 404,
        status,
    );
    checks.check(
        "the folder is logged as the catalog's source",
        server
            .saw(
                0,
                &format!("catalog from the local repository {folder} (1 application)."),
                Duration::from_secs(5),
            )
            .await,
        "",
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn proot_apps_own_variable_names_the_repository_as_well() {
    let fixture = Fixture::new("variable");
    let mut checks = Checks::new("apps-local-repo");
    let server = fixture.server("variable", Some("PA_REPO_FOLDER")).await;
    let published = local_repo_published(&server).await;
    let (status, _) = get(&server, "/api/apps/metadata.yml").await;
    checks.check(
        "PA_REPO_FOLDER alone names the repository",
        published == Some(Value::Bool(true)) && status == 200,
        format!("apps_local_repo={published:?}, catalog {status}"),
    );
    checks.finish(&server.stop().await);
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn without_a_repository_the_page_reads_the_remote_catalog() {
    let fixture = Fixture::new("remote");
    let mut checks = Checks::new("apps-local-repo");
    let server = fixture.server("remote", None).await;
    let published = local_repo_published(&server).await;
    checks.check(
        "without a repository the page reads the remote catalog",
        published == Some(Value::Bool(false)),
        format!("apps_local_repo={published:?}"),
    );
    let (status, _) = get(&server, "/api/apps/metadata.yml").await;
    checks.check("and nothing is served here", status == 404, status);
    checks.finish(&server.stop().await);
}
