/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The apps panel's catalog when proot-apps reads a local repository.
//!
//! proot-apps takes `PA_REPO_FOLDER` as a folder of application tarballs in place of its
//! remote registry: one directory per application, named for the image with the separators
//! replaced (`ghcr.io_linuxserver_proot-apps_firefox`), and optionally a `metadata/` directory
//! holding the catalog its graphical installer shows (`metadata.yml` and the icons under
//! `img/`). A session on such a repository installs only what the folder holds, so a panel
//! listing the remote catalog would offer applications every install of which fails. The
//! server therefore serves the folder's own catalog over `GET /api/apps/metadata.yml` and
//! `GET /api/apps/img/{name}`, behind the session's own authentication, and tells the page to
//! read it there (`apps_local_repo` in the settings payload).
//!
//! A repository without `metadata/` is still a repository: its catalog is written here from
//! the application directories, the listing proot-apps' own shell completion offers, each
//! named as the runner takes it on the command line (the short name for the default namespace,
//! the image reference otherwise) and nothing more, since nothing more about them is known.
//! The document is JSON, which is YAML to the page's parser. Nothing is served where no
//! repository is configured, since the page then reads the remote catalog; a configured
//! directory that is missing is the same answer, and the panel reports the catalog as failed
//! to load rather than offering installs the runner cannot make.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::confine::realpath;
use super::{FilesState, text};

/// The directory of a repository's catalog.
pub const METADATA_DIR: &str = "metadata";
/// The catalog file.
pub const METADATA_FILE: &str = "metadata.yml";
/// The icons' directory under the catalog's.
pub const IMAGE_DIR: &str = "img";
/// The image folders proot-apps names by their short name alone.
pub const DEFAULT_IMAGE_PREFIX: &str = "ghcr.io_linuxserver_proot-apps_";

/// The catalog file a local repository may carry.
pub fn metadata_file(folder: &Path) -> PathBuf {
    folder.join(METADATA_DIR).join(METADATA_FILE)
}

/// The command-line name of an application directory, as proot-apps resolves it: a directory
/// under the default namespace is the short application name; any other is the image
/// reference, `registry/namespace/image:tag`.
pub fn app_name(image_folder: &str) -> String {
    if let Some(short) = image_folder.strip_prefix(DEFAULT_IMAGE_PREFIX) {
        return short.to_string();
    }
    match image_folder.rsplit_once('_') {
        Some((head, tag)) => format!("{}:{tag}", head.replace('_', "/")),
        None => image_folder.to_string(),
    }
}

/// The applications a local repository holds, by command-line name; loose files and the
/// metadata directory are not applications.
pub fn listed_apps(folder: &Path) -> Vec<String> {
    let Ok(listing) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut names: Vec<String> = listing
        .flatten()
        .filter(|e| e.file_name() != METADATA_DIR && e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().map(|n| app_name(&n)).collect()
}

/// The catalog document of a local repository, or `None` where there is no repository: the
/// repository's own `metadata/metadata.yml` verbatim when it has one, else a document naming
/// the applications the folder holds.
pub fn catalog_text(folder: &str) -> Option<String> {
    if folder.is_empty() {
        return None;
    }
    let folder = Path::new(folder);
    if !folder.is_dir() {
        return None;
    }
    if let Ok(text) = std::fs::read_to_string(metadata_file(folder)) {
        return Some(text);
    }
    let include: Vec<serde_json::Value> = listed_apps(folder)
        .into_iter()
        .map(|name| json!({"name": name, "full_name": name, "description": "", "icon": ""}))
        .collect();
    let mut document =
        serde_json::to_string_pretty(&json!({"include": include})).unwrap_or_else(|_| "{}".into());
    document.push('\n');
    Some(document)
}

/// The file behind one catalog icon, or `None` where there is none to serve: `name` is a file
/// name the catalog gave, so a value that is not one (a path, a traversal, a NUL byte, a name
/// resolving outside the image directory through a link) names nothing.
pub fn icon_path(folder: &str, name: &str) -> Option<PathBuf> {
    if folder.is_empty()
        || name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
    {
        return None;
    }
    let images = realpath(&Path::new(folder).join(METADATA_DIR).join(IMAGE_DIR));
    let path = realpath(&images.join(name));
    let mut prefix = images.as_os_str().to_os_string();
    prefix.push("/");
    if !path
        .as_os_str()
        .as_encoded_bytes()
        .starts_with(prefix.as_encoded_bytes())
    {
        return None;
    }
    if path.parent() != Some(images.as_path()) || !path.is_file() {
        return None;
    }
    Some(path)
}

/// `GET /api/apps/metadata.yml`.
pub async fn handle_catalog(State(state): State<Arc<FilesState>>) -> Response {
    let folder = state.config.apps_repo.clone();
    let Ok(text_or_none) = tokio::task::spawn_blocking(move || catalog_text(&folder)).await else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "catalog failed");
    };
    match text_or_none {
        None => text(StatusCode::NOT_FOUND, "No local apps repository"),
        Some(document) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/yaml; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            document,
        )
            .into_response(),
    }
}

/// `GET /api/apps/img/{name}`: one icon of the local repository's catalog, served from its
/// `metadata/img` directory and nowhere else.
pub async fn handle_icon(
    State(state): State<Arc<FilesState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let folder = state.config.apps_repo.clone();
    let Ok(path) = tokio::task::spawn_blocking(move || icon_path(&folder, &name)).await else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "icon failed");
    };
    let Some(path) = path else {
        return text(StatusCode::NOT_FOUND, "No such icon");
    };
    let (parts, _) = request.into_parts();
    let served = Request::from_parts(parts, Body::empty());
    match ServeFile::new(path).oneshot(served).await {
        Ok(response) => response.map(Body::new),
        Err(_) => text(StatusCode::NOT_FOUND, "No such icon"),
    }
}

#[cfg(test)]
mod tests {
    //! The repository's own metadata is the catalog, a repository without one lists what it holds,
    //! and icons come from the image directory alone.

    use super::*;
    use std::os::unix::fs::symlink;

    const METADATA: &str = "include:\n  - name: firefox\n    full_name: Firefox\n    description: Browser\n    icon: firefox.svg\n";

    fn repository(root: &Path, metadata: bool) -> PathBuf {
        let folder = root.join("apps");
        for image in [
            "ghcr.io_linuxserver_proot-apps_firefox",
            "ghcr.io_myorg_proot-apps_tool",
        ] {
            std::fs::create_dir_all(folder.join(image)).unwrap();
        }
        std::fs::write(folder.join("SHALAYER"), "loose file\n").unwrap();
        if metadata {
            std::fs::create_dir_all(folder.join("metadata/img")).unwrap();
            std::fs::write(folder.join("metadata/metadata.yml"), METADATA).unwrap();
            std::fs::write(folder.join("metadata/img/firefox.svg"), "<svg/>").unwrap();
        }
        folder
    }

    #[test]
    fn a_repository_with_metadata() {
        let root = super::super::testing::scratch("selkies-apps");
        let folder = repository(&root, true);
        let folder_str = folder.to_str().unwrap();
        assert_eq!(
            catalog_text(folder_str).as_deref(),
            Some(METADATA),
            "metadata is the catalog, verbatim"
        );
        assert_eq!(
            icon_path(folder_str, "firefox.svg"),
            Some(folder.join("metadata/img/firefox.svg"))
        );
        assert_eq!(icon_path(folder_str, "missing.svg"), None);
        for name in [
            "../metadata.yml",
            "..",
            "img/../../SHALAYER",
            "",
            "firefox.svg\0.png",
        ] {
            assert_eq!(
                icon_path(folder_str, name),
                None,
                "not a file name: {name:?}"
            );
        }
        symlink(folder.join("SHALAYER"), folder.join("metadata/img/out.svg")).unwrap();
        assert_eq!(
            icon_path(folder_str, "out.svg"),
            None,
            "a link leading out of the image directory"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_repository_without_metadata_names_what_it_holds() {
        let root = super::super::testing::scratch("selkies-apps");
        let folder = repository(&root, false);
        let document = catalog_text(folder.to_str().unwrap()).expect("a catalog");
        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        let names: Vec<&str> = parsed["include"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["firefox", "ghcr.io/myorg/proot-apps:tool"]);
        assert_eq!(catalog_text(root.join("absent").to_str().unwrap()), None);
        assert_eq!(catalog_text(""), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
