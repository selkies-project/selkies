/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The apps catalog: where the panel and Pelorus read it, and what it lists.
//!
//! The catalog is a `metadata.yml` in the format of the proot-apps repository's own, a list
//! under `include` of entries naming the application (`name`, what the runner takes), its shown
//! name, description, icon, and category, with `disabled` for one listed but not installable;
//! its icons sit in `img/` beside it. Where it is read is one setting's reading
//! ([`AppsCatalog`]): a remote catalog the page fetches itself, or one this server serves under
//! `GET /api/apps/metadata.yml` and `GET /api/apps/img/{name}`, behind the session's own
//! authentication, which the page is told by an empty `apps_catalog` in the settings payload.
//!
//! The server serves two kinds. A catalog file the operator names is served as it stands, its
//! icons from `img/` beside it. A proot-apps local repository (`PA_REPO_FOLDER`), a folder of
//! application tarballs in place of the remote registry, one directory per application named
//! for the image with the separators replaced (`ghcr.io_linuxserver_proot-apps_firefox`), takes
//! precedence over any catalog setting: a session on one installs only what the folder holds,
//! so a panel listing another catalog would offer applications every install of which fails.
//! Its own `metadata/` directory is the catalog where it carries one; a repository without one
//! is still a repository, and its catalog is written here from the application directories,
//! the listing proot-apps' own shell completion offers, each named as the runner takes it on
//! the command line (the short name for the default namespace, the image reference otherwise)
//! and nothing more, since nothing more about them is known. That document is JSON, which is
//! YAML to the page's parser. Nothing is served for a remote catalog; a configured file or
//! directory that is missing is the same answer, and the panel reports the catalog as failed to
//! load rather than offering installs the runner cannot make.
//!
//! [`parse_catalog`] reads a catalog by the rule the panel lists by, for a reader in the server.

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
use crate::settings::AppsCatalog;

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

/// The catalog document this server serves, or `None` where it serves none: a local
/// repository's own `metadata/metadata.yml` verbatim when it has one, else a document naming the
/// applications the folder holds; a catalog file as it stands; nothing for a remote catalog.
pub fn catalog_text(source: &AppsCatalog) -> Option<String> {
    match source {
        AppsCatalog::Repo(folder) => repo_catalog_text(Path::new(folder)),
        AppsCatalog::File(file) if !file.is_empty() => std::fs::read_to_string(file).ok(),
        AppsCatalog::File(_) | AppsCatalog::Remote(_) => None,
    }
}

/// A local repository's catalog document, `None` where the folder is missing.
fn repo_catalog_text(folder: &Path) -> Option<String> {
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

/// The directory this server serves the catalog's icons from, `None` where it serves none.
fn images_dir(source: &AppsCatalog) -> Option<PathBuf> {
    match source {
        AppsCatalog::Repo(folder) => Some(Path::new(folder).join(METADATA_DIR).join(IMAGE_DIR)),
        AppsCatalog::File(file) if !file.is_empty() => {
            Some(Path::new(file).parent()?.join(IMAGE_DIR))
        }
        AppsCatalog::File(_) | AppsCatalog::Remote(_) => None,
    }
}

/// The file behind one catalog icon, or `None` where there is none to serve: `name` is a file
/// name the catalog gave, so a value that is not one (a path, a traversal, a NUL byte, a name
/// resolving outside the image directory through a link) names nothing.
pub fn icon_path(source: &AppsCatalog, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return None;
    }
    let images = realpath(&images_dir(source)?);
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

/// One application a catalog lists as installable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogApp {
    /// What the runner takes on its command line.
    pub name: String,
    pub full_name: String,
    pub description: String,
    /// The category it is filed under, empty where the catalog names none.
    pub category: String,
}

/// The installable applications of a catalog document, by the rule the panel lists by: an
/// entry under `include` with a string `name`, `full_name`, `description`, and `icon`, a string
/// `category` and a boolean `disabled` where it has them (an empty value, YAML's null, is
/// neither), and not `disabled`. An entry of any other shape is passed over
/// rather than failing the catalog; a document that is not YAML, or has no `include` list, is
/// the error.
pub fn parse_catalog(text: &str) -> Result<Vec<CatalogApp>, String> {
    let document: serde_json::Value =
        serde_saphyr::from_str(text).map_err(|e| format!("the catalog is not YAML: {e}"))?;
    let entries = document
        .get("include")
        .and_then(serde_json::Value::as_array)
        .ok_or("the catalog lists no applications under `include`")?;
    let string = |entry: &serde_json::Value, key: &str| -> Option<String> {
        entry.get(key)?.as_str().map(str::to_string)
    };
    Ok(entries
        .iter()
        .filter_map(|entry| {
            let category = match entry.get("category") {
                None => String::new(),
                Some(value) => value.as_str()?.to_string(),
            };
            let disabled = match entry.get("disabled") {
                None => false,
                Some(value) => value.as_bool()?,
            };
            string(entry, "icon")?;
            let app = CatalogApp {
                name: string(entry, "name")?,
                full_name: string(entry, "full_name")?,
                description: string(entry, "description")?,
                category,
            };
            (!disabled).then_some(app)
        })
        .collect())
}

/// `GET /api/apps/metadata.yml`.
pub async fn handle_catalog(State(state): State<Arc<FilesState>>) -> Response {
    let source = state.config.apps_catalog.clone();
    let Ok(text_or_none) = tokio::task::spawn_blocking(move || catalog_text(&source)).await else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "catalog failed");
    };
    match text_or_none {
        None => text(StatusCode::NOT_FOUND, "No apps catalog served here"),
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
    let source = state.config.apps_catalog.clone();
    let Ok(path) = tokio::task::spawn_blocking(move || icon_path(&source, &name)).await else {
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
    //! a catalog file is served with the icons beside it, icons come from the image directory
    //! alone, and a catalog lists what the panel would.

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

    fn repo(folder: &Path) -> AppsCatalog {
        AppsCatalog::Repo(folder.to_str().unwrap().to_string())
    }

    #[test]
    fn a_repository_with_metadata() {
        let root = super::super::testing::scratch("selkies-apps");
        let folder = repository(&root, true);
        let source = repo(&folder);
        assert_eq!(
            catalog_text(&source).as_deref(),
            Some(METADATA),
            "metadata is the catalog, verbatim"
        );
        assert_eq!(
            icon_path(&source, "firefox.svg"),
            Some(folder.join("metadata/img/firefox.svg"))
        );
        assert_eq!(icon_path(&source, "missing.svg"), None);
        for name in [
            "../metadata.yml",
            "..",
            "img/../../SHALAYER",
            "",
            "firefox.svg\0.png",
        ] {
            assert_eq!(icon_path(&source, name), None, "not a file name: {name:?}");
        }
        symlink(folder.join("SHALAYER"), folder.join("metadata/img/out.svg")).unwrap();
        assert_eq!(
            icon_path(&source, "out.svg"),
            None,
            "a link leading out of the image directory"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_repository_without_metadata_names_what_it_holds() {
        let root = super::super::testing::scratch("selkies-apps");
        let folder = repository(&root, false);
        let document = catalog_text(&repo(&folder)).expect("a catalog");
        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        let names: Vec<&str> = parsed["include"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["firefox", "ghcr.io/myorg/proot-apps:tool"]);
        assert_eq!(catalog_text(&repo(&root.join("absent"))), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_catalog_file_is_served_with_the_icons_beside_it() {
        let root = super::super::testing::scratch("selkies-apps");
        std::fs::create_dir_all(root.join("img")).unwrap();
        std::fs::write(root.join("custom.yml"), METADATA).unwrap();
        std::fs::write(root.join("img/firefox.svg"), "<svg/>").unwrap();
        let source = AppsCatalog::File(root.join("custom.yml").to_str().unwrap().to_string());
        assert_eq!(catalog_text(&source).as_deref(), Some(METADATA));
        assert_eq!(
            icon_path(&source, "firefox.svg"),
            Some(root.join("img/firefox.svg"))
        );
        assert_eq!(icon_path(&source, "../custom.yml"), None);
        let missing = AppsCatalog::File(root.join("absent.yml").to_str().unwrap().to_string());
        assert_eq!(catalog_text(&missing), None);
        for none in [
            AppsCatalog::File(String::new()),
            AppsCatalog::Remote("https://example.com/metadata.yml".into()),
        ] {
            assert_eq!(catalog_text(&none), None, "{none:?}");
            assert_eq!(icon_path(&none, "firefox.svg"), None, "{none:?}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_catalog_lists_the_entries_the_panel_would() {
        let text = "include:\n\
            \x20 - name: firefox\n    full_name: Firefox\n    arch: linux/amd64\n    category: Network\n    icon: firefox.svg\n    description: \"A browser.\"\n\
            \x20 - name: anki\n    full_name: Anki\n    icon: anki.svg\n    description: Cards\n    disabled: True\n\
            \x20 - name: kept\n    full_name: Kept\n    icon: kept.svg\n    description: Not disabled\n    disabled: false\n\
            \x20 - name: noicon\n    full_name: No icon\n    description: Malformed\n\
            \x20 - name: 7\n    full_name: Numbered\n    icon: n.svg\n    description: Malformed\n\
            \x20 - name: blank\n    full_name: Blank\n    icon: b.svg\n    description: Null category\n    category:\n";
        let apps = parse_catalog(text).expect("a catalog");
        assert_eq!(
            apps,
            [
                CatalogApp {
                    name: "firefox".into(),
                    full_name: "Firefox".into(),
                    description: "A browser.".into(),
                    category: "Network".into(),
                },
                CatalogApp {
                    name: "kept".into(),
                    full_name: "Kept".into(),
                    description: "Not disabled".into(),
                    category: String::new(),
                },
            ]
        );
        let root = super::super::testing::scratch("selkies-apps");
        let written = catalog_text(&repo(&repository(&root, false))).unwrap();
        assert_eq!(
            parse_catalog(&written).unwrap().len(),
            2,
            "the document written for a repository without metadata"
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(parse_catalog("apps: []\n").is_err());
        assert!(parse_catalog("include: [unclosed\n").is_err());
    }
}
