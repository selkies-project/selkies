/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The route cases of the confinement rules, the index's parent link, and printing, driven through
//! the router under a fixed auth.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use serde_json::Value as Json;

use super::testing::{Recorder, Who, call, get, json, scratch, state};

fn names(body: &Json) -> Vec<String> {
    body["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| e["name"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn zip_entries(bytes: &[u8]) -> Vec<(String, u32)> {
    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).unwrap();
        out.push((
            entry.name().unwrap().to_string(),
            entry.unix_mode().unwrap_or(0) & 0o7777,
        ));
    }
    out.sort();
    out
}

#[tokio::test]
async fn browse_mkdir_upload_and_zip_hold_to_the_root() {
    let root = scratch("selkies-confinement");
    let recorder = Arc::new(Recorder::default());
    let server = state(&root, recorder.clone());
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::write(root.join("real/plain.txt"), b"payload").unwrap();
    symlink("real", root.join("docs")).unwrap();
    std::fs::write(root.join("plain.txt"), b"payload").unwrap();
    std::fs::create_dir_all(root.join("missing/deep")).unwrap();
    let outside = root.parent().unwrap().to_path_buf();

    let (status, _, _) = get(&server, Who::Viewer, "/file-manager/browse", &[]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "browse: a viewer is refused the listing"
    );
    let (status, _, body) = get(&server, Who::Controller, "/file-manager/browse", &[]).await;
    let listed = names(&json(&body));
    assert_eq!(status, StatusCode::OK);
    for expected in ["real", "docs", "plain.txt", "missing"] {
        assert!(
            listed.iter().any(|n| n == expected),
            "browse: the root lists {expected}: {listed:?}"
        );
    }
    let (status, _, body) = get(
        &server,
        Who::Controller,
        "/file-manager/browse?path=docs",
        &[],
    )
    .await;
    assert_eq!(
        (status, names(&json(&body))),
        (StatusCode::OK, vec!["plain.txt".to_string()]),
        "an in-tree link lists"
    );
    let (status, _, _) = get(
        &server,
        Who::Controller,
        "/file-manager/browse?path=plain.txt",
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "browse: a file is not a directory"
    );
    let (status, _, _) = get(
        &server,
        Who::Controller,
        "/file-manager/browse?path=../..",
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "browse: a traversal is refused"
    );
    let (status, _, _) = get(
        &server,
        Who::Controller,
        "/file-manager/browse?path=real%00x",
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "browse: a NUL byte is refused, not a 500"
    );
    let (status, _, body) = get(
        &server,
        Who::Controller,
        "/file-manager/browse?path=.//real/",
        &[],
    )
    .await;
    let body = json(&body);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path"], "real", "entry paths are the normalized ones");
    assert_eq!(body["entries"][0]["path"], "real/plain.txt");

    let mkdir = |who: Who, path: &str, extra: Vec<(&'static str, &'static str)>| {
        let server = server.clone();
        let body = format!("{{\"path\": {}}}", serde_json::to_string(path).unwrap());
        async move {
            let mut headers = vec![("content-type", "application/json")];
            headers.extend(extra);
            call(
                &server,
                who,
                Method::POST,
                "/file-manager/mkdir",
                &headers,
                Body::from(body),
            )
            .await
            .0
        }
    };
    assert_eq!(
        mkdir(Who::Controller, "", vec![]).await,
        StatusCode::BAD_REQUEST,
        "mkdir: an empty path is refused"
    );
    assert_eq!(
        mkdir(Who::Controller, "sub dir/..", vec![]).await,
        StatusCode::BAD_REQUEST,
        "mkdir: a root spelling"
    );
    assert_eq!(
        mkdir(Who::Viewer, "viewer-dir", vec![]).await,
        StatusCode::FORBIDDEN,
        "mkdir: a viewer is refused"
    );
    assert_eq!(
        mkdir(Who::Controller, "deep/nested/dir", vec![]).await,
        StatusCode::OK
    );
    assert!(
        root.join("deep/nested/dir").is_dir(),
        "mkdir: the missing intermediates are made with the leaf"
    );
    assert_eq!(
        mkdir(Who::Controller, "deep/nested/dir", vec![]).await,
        StatusCode::CONFLICT,
        "one that exists"
    );
    assert_eq!(
        mkdir(Who::Controller, "plain.txt/below", vec![]).await,
        StatusCode::CONFLICT,
        "a file in the way"
    );
    assert_eq!(
        mkdir(
            Who::Controller,
            "cross-site-dir",
            vec![("sec-fetch-site", "cross-site")]
        )
        .await,
        StatusCode::FORBIDDEN,
        "mkdir: another site's page is refused"
    );
    assert!(!root.join("cross-site-dir").exists());
    symlink(&outside, root.join("away")).unwrap();
    assert_eq!(
        mkdir(Who::Controller, "away/evil", vec![]).await,
        StatusCode::BAD_REQUEST,
        "a link out of the tree"
    );
    assert!(
        !outside.join("evil").exists(),
        "mkdir: the creation did not follow the link out"
    );

    let payload = b"plain upload bytes".to_vec();
    let (status, _, _) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/upload",
        &[("x-upload-path", "up%20dir/file.bin")],
        Body::from(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::fs::read(root.join("up dir/file.bin")).unwrap(),
        payload,
        "upload: a plain body lands under its name"
    );
    let (status, _, _) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/upload",
        &[("x-upload-path", "away/evil.bin")],
        Body::from("x"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "upload: a link out of the tree is refused"
    );
    assert!(!outside.join("evil.bin").exists());
    let (status, _, _) = call(
        &server,
        Who::Viewer,
        Method::POST,
        "/upload",
        &[("x-upload-path", "viewer.bin")],
        Body::from("x"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "upload: a viewer is refused");

    let (head, tail) = (b"hello ".to_vec(), b"world!!".to_vec());
    let total = (head.len() + tail.len()).to_string();
    let (status, _, body) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/upload",
        &[
            ("x-upload-path", "chunked.bin"),
            ("x-upload-id", "t1"),
            ("x-upload-offset", "0"),
            ("x-upload-total", &total),
        ],
        Body::from(head.clone()),
    )
    .await;
    assert_eq!(
        (status, json(&body)["complete"].as_bool()),
        (StatusCode::OK, Some(false)),
        "the first slice is banked"
    );
    let offset = head.len().to_string();
    let (status, _, _) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/upload",
        &[
            ("x-upload-path", "chunked.bin"),
            ("x-upload-id", "t1"),
            ("x-upload-offset", &offset),
            ("x-upload-total", &total),
            ("x-upload-final", "1"),
        ],
        Body::from(tail.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        std::fs::read(root.join("chunked.bin")).unwrap(),
        [head, tail].concat(),
        "the final slice renames the staged file"
    );

    let (status, _, archived) =
        get(&server, Who::Controller, "/file-manager/zip?path=real", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        archived.starts_with(b"PK") && zip_entries(&archived).iter().any(|(n, _)| n == "plain.txt"),
        "zip: one archive"
    );
    let (status, headers, _) = get(
        &server,
        Who::Controller,
        "/file-manager/zip?path=real",
        &[("range", "bytes=3-")],
    )
    .await;
    assert_eq!(
        (status, headers.get("allow").map(|v| v.to_str().unwrap())),
        (StatusCode::METHOD_NOT_ALLOWED, Some("GET"))
    );
    let (status, _, _) = get(&server, Who::Controller, "/file-manager/zip", &[]).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "zip: the root itself is refused"
    );
    let (status, _, archived) =
        get(&server, Who::Controller, "/file-manager/zip?path=docs", &[]).await;
    assert!(
        status == StatusCode::OK && archived.starts_with(b"PK"),
        "zip: an in-tree link is followed"
    );
    let (status, _, _) = get(&server, Who::Viewer, "/file-manager/zip?path=real", &[]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "zip: a viewer is refused the archive"
    );
    let (status, _, _) = get(
        &server,
        Who::Controller,
        "/file-manager/zip?path=real",
        &[("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "zip: another site's page is refused before anything is packed"
    );
    let (status, _, _) = get(
        &server,
        Who::Controller,
        "/file-manager/zip?path=plain.txt",
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "zip: a file rather than a folder is not found"
    );

    let secret_file = outside.join("zip-outside.txt");
    std::fs::write(&secret_file, b"OUTSIDE-SECRET").unwrap();
    symlink(&secret_file, root.join("real/out-link")).unwrap();
    symlink("plain.txt", root.join("real/in-link")).unwrap();
    nix::unistd::mkfifo(
        &root.join("real/pipe"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    std::fs::write(root.join("real/old.txt"), b"old").unwrap();
    std::fs::File::options()
        .write(true)
        .open(root.join("real/old.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH)
        .unwrap();
    let (status, _, archived) =
        get(&server, Who::Controller, "/file-manager/zip?path=real", &[]).await;
    let packed: Vec<String> = zip_entries(&archived).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        (status, packed),
        (
            StatusCode::OK,
            vec!["old.txt".to_string(), "plain.txt".to_string()]
        ),
        "links and a FIFO out"
    );
    assert!(!archived.windows(14).any(|w| w == b"OUTSIDE-SECRET"));
    for name in ["real/out-link", "real/in-link", "real/pipe", "real/old.txt"] {
        std::fs::remove_file(root.join(name)).unwrap();
    }
    std::fs::remove_file(&secret_file).unwrap();

    let secret = outside.join("listing-outside.bin");
    std::fs::write(&secret, vec![b'x'; 12345]).unwrap();
    symlink(&secret, root.join("out-file")).unwrap();
    symlink(&outside, root.join("out-dir")).unwrap();
    let (status, _, body) = get(&server, Who::Controller, "/file-manager/browse", &[]).await;
    let body = json(&body);
    let entry = |name: &str| {
        body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(status, StatusCode::OK);
    assert!(
        entry("out-file")["size"]
            .as_u64()
            .is_some_and(|s| s != 12345),
        "a link out is listed as itself"
    );
    assert_eq!(entry("out-dir")["is_dir"], false);
    assert_eq!(entry("docs")["is_dir"], true);
    std::fs::remove_file(root.join("out-file")).unwrap();
    std::fs::remove_file(root.join("out-dir")).unwrap();
    std::fs::remove_file(&secret).unwrap();

    server.listing_limit.store(1, Ordering::Relaxed);
    let (status, _, _) = get(&server, Who::Controller, "/file-manager/browse", &[]).await;
    server
        .listing_limit
        .store(super::confine::FILE_LISTING_MAX_ENTRIES, Ordering::Relaxed);
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "browse: a folder past the listing bound is refused with 413"
    );

    std::fs::create_dir_all(root.join("line\nbreak")).unwrap();
    let tool = root.join("line\nbreak/tool");
    std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o4755)).unwrap();
    let odd = std::ffi::OsString::from_vec_lossless(b"odd-\xff.bin");
    std::fs::write(root.join("line\nbreak").join(&odd), b"odd").unwrap();
    let (status, headers, archived) = get(
        &server,
        Who::Controller,
        "/file-manager/zip?path=line%0Abreak",
        &[],
    )
    .await;
    let modes = zip_entries(&archived);
    assert_eq!(status, StatusCode::OK);
    assert!(
        modes.iter().any(|(n, m)| n == "tool" && *m == 0o755),
        "no set-ID bit rides into the archive: {modes:?}"
    );
    assert!(
        modes.iter().any(|(n, _)| n == "odd-\u{fffd}.bin"),
        "a name that is not UTF-8 is packed: {modes:?}"
    );
    let disposition = headers
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        disposition.contains("filename=\"line_break.zip\"") && !disposition.contains('\n'),
        "{disposition}"
    );
    std::fs::remove_dir_all(root.join("line\nbreak")).unwrap();

    let long = "n".repeat(300);
    let body = format!("{{\"path\": \"{long}\"}}");
    let (status, _, body) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/file-manager/mkdir",
        &[("content-type", "application/json")],
        Body::from(body),
    )
    .await;
    let message = json(&body)["message"].as_str().unwrap_or("").to_string();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !message.is_empty() && !message.contains(root.to_str().unwrap()),
        "an error names the failure, not the paths"
    );

    server.archive_entry_limit.store(0, Ordering::Relaxed);
    let (status, _, _) = get(&server, Who::Controller, "/file-manager/zip?path=real", &[]).await;
    server
        .archive_entry_limit
        .store(super::archive::ARCHIVE_MAX_ENTRIES, Ordering::Relaxed);
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "zip: a folder past the bound is refused with 413"
    );

    let events = recorder.events.lock().clone();
    assert!(events.iter().any(|(e, f)| e == "file.upload.end"
        && f["filename"] == "up dir/file.bin"
        && f["size_bytes"] == 18));
    assert!(
        events
            .iter()
            .any(|(e, f)| e == "file.mkdir" && f["filename"] == "deep/nested/dir")
    );
    assert!(
        events
            .iter()
            .any(|(e, f)| e == "file.upload.error" && f["filename"] == "viewer.bin")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// `std::ffi::OsString` from raw bytes, for a name that is not UTF-8.
trait FromBytes {
    fn from_vec_lossless(bytes: &[u8]) -> std::ffi::OsString;
}

impl FromBytes for std::ffi::OsString {
    fn from_vec_lossless(bytes: &[u8]) -> std::ffi::OsString {
        std::os::unix::ffi::OsStringExt::from_vec(bytes.to_vec())
    }
}

#[tokio::test]
async fn the_index_carries_a_parent_row_below_the_root_alone() {
    let root = scratch("selkies-file-index");
    let server = state(&root, Arc::new(Recorder::default()));
    std::fs::create_dir_all(root.join("sub/inner")).unwrap();
    std::fs::write(root.join("sub/a.txt"), "x").unwrap();
    let parent_rows = |body: &[u8]| {
        String::from_utf8_lossy(body)
            .matches("<a href=\"../\"")
            .count()
    };

    let (status, _, body) = get(&server, Who::Controller, "/files/", &[]).await;
    assert_eq!(status, StatusCode::OK, "root listing answers 200");
    assert_eq!(parent_rows(&body), 0, "root listing has no parent row");
    assert!(
        String::from_utf8_lossy(&body).contains("<a href=\"sub/\">"),
        "root listing shows the subdirectory"
    );

    let (status, _, body) = get(&server, Who::Controller, "/files/sub/", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        parent_rows(&body),
        1,
        "subdirectory listing has exactly one parent row"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.find("href=\"../\"").unwrap() < text.find("a.txt").unwrap(),
        "parent row precedes the entries"
    );

    let (_, _, body) = get(&server, Who::Controller, "/files/sub/inner/", &[]).await;
    assert_eq!(
        parent_rows(&body),
        1,
        "nested listing has exactly one parent row"
    );

    let (status, headers, _) = get(&server, Who::Controller, "/files/sub?token=abc", &[]).await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        headers.get("location").unwrap(),
        "/api/files/sub/?token=abc",
        "slashless directory redirects to its slash form"
    );

    let (status, _, _) = get(&server, Who::Controller, "/files/..%2f", &[]).await;
    assert!(
        matches!(status, StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN),
        "traversal stays refused: {status}"
    );

    let (status, headers, body) = get(&server, Who::Controller, "/files/sub/a.txt", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"x");
    assert!(
        headers
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("attachment; filename=\"a.txt\"")
    );
    let (status, _, _) = get(&server, Who::Viewer, "/files/", &[]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn the_footer_function_is_the_nginx_footers() {
    let body = |text: &str| {
        let start = text.find("function processDirectoryListing()").unwrap();
        let end = text[start..].find("let attempts = 0;").unwrap() + start;
        text[start..end].to_string()
    };
    let nginx = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../web/selkies-web-core/nginx/footer.html"
    ))
    .expect("the nginx footer beside the crate");
    assert_eq!(
        body(&nginx),
        body(super::index_html::FILE_INDEX_FOOTER),
        "both footer copies carry the same function"
    );
}

#[tokio::test]
async fn printed_documents_are_served_once_and_taken_from_the_spool() {
    let root = scratch("selkies-printing-route");
    let recorder = Arc::new(Recorder::default());
    let server = state(&root, recorder.clone());
    let spool = server.print_spool().clone();
    std::fs::create_dir_all(&spool).unwrap();
    server.set_print_watching(true);
    let body: Vec<u8> = [
        b"%PDF-1.4 ".to_vec(),
        (0..2048u32).map(|i| (i % 251) as u8).collect(),
    ]
    .concat();
    std::fs::write(spool.join("Report.pdf"), &body).unwrap();

    let (status, _, _) = call(
        &server,
        Who::Controller,
        Method::HEAD,
        "/print/Report.pdf",
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "HEAD answers");
    assert!(
        spool.join("Report.pdf").exists() && recorder.events.lock().is_empty(),
        "without taking the document"
    );
    let (status, headers, got) = get(&server, Who::Controller, "/print/Report.pdf", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, body, "the document is served");
    assert_eq!(headers.get("content-type").unwrap(), "application/pdf");
    assert_eq!(headers.get("content-disposition").unwrap(), "inline");
    assert_eq!(
        recorder.events.lock().clone(),
        vec![(
            "print.document".to_string(),
            serde_json::json!({"filename": "Report.pdf", "size_bytes": body.len(), "partial": false})
        )],
        "the audit records it with the bytes served"
    );
    assert!(
        !spool.join("Report.pdf").exists(),
        "a document that went out whole leaves the spool"
    );
    let (status, _, _) = get(&server, Who::Controller, "/print/Report.pdf", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "and is gone the second time");
    assert!(
        server.pending_print_documents().is_empty(),
        "pending is empty once taken"
    );

    std::fs::write(spool.join("Range.pdf"), &body).unwrap();
    let (status, _, _) = get(
        &server,
        Who::Controller,
        "/print/Range.pdf",
        &[("range", "bytes=0-99")],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        recorder.events.lock().last().cloned(),
        Some((
            "print.document".to_string(),
            serde_json::json!({"filename": "Range.pdf", "size_bytes": 100, "partial": true})
        )),
        "a range is recorded as partial"
    );
    assert!(spool.join("Range.pdf").exists(), "and the document stays");
    assert_eq!(
        server.pending_print_documents(),
        vec![("Range.pdf".to_string(), body.len() as u64)],
        "pending names it"
    );

    let (status, _, _) = get(&server, Who::Viewer, "/print/Range.pdf", &[]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "view-only credentials are refused"
    );
    for headers in [
        vec![("sec-fetch-site", "cross-site")],
        vec![("sec-fetch-site", "same-site")],
        vec![
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://elsewhere.example"),
        ],
        vec![("origin", "https://elsewhere.example")],
    ] {
        let (status, _, _) = get(&server, Who::Controller, "/print/Range.pdf", &headers).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "another site's page is refused ({headers:?})"
        );
        assert!(spool.join("Range.pdf").exists());
    }
    let (status, _, _) = call(
        &server,
        Who::Controller,
        Method::HEAD,
        "/print/Range.pdf",
        &[("sec-fetch-site", "same-origin")],
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the session's own page is not");
    for bad in ["..%2FRange.pdf", "notes.txt", ".hidden.pdf", "missing.pdf"] {
        let (status, _, _) = get(&server, Who::Controller, &format!("/print/{bad}"), &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad} is not served");
    }
    server.set_print_watching(false);
    assert!(
        server.pending_print_documents().is_empty(),
        "without a watcher nothing is pending"
    );

    let disabled = {
        let config = super::FilesConfig {
            printing_enabled: false,
            ..server.config().clone()
        };
        Arc::new(super::FilesState::new(config, super::Hooks::default()))
    };
    let (status, _, _) = get(&disabled, Who::Controller, "/print/Range.pdf", &[]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a disabled policy refuses every document"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn recording_status_reports_none_before_any_recording() {
    let root = scratch("selkies-recording-route");
    let server = state(&root, Arc::new(Recorder::default()));
    let (status, _, body) = get(&server, Who::Viewer, "/recording", &[]).await;
    assert_eq!(
        (status, json(&body)["active"].as_bool()),
        (StatusCode::OK, Some(false)),
        "a viewer may read it"
    );
    let (status, _, _) = call(
        &server,
        Who::Viewer,
        Method::POST,
        "/recording",
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a viewer cannot record");
    let (status, _, body) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/recording",
        &[],
        Body::from("{\"path\": \"../out.mp4\"}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        String::from_utf8_lossy(&body),
        "Recording path leaves the file-manager directory"
    );
    let (status, _, body) = call(
        &server,
        Who::Controller,
        Method::POST,
        "/recording",
        &[],
        Body::from("not json"),
    )
    .await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).into_owned()),
        (StatusCode::BAD_REQUEST, "Body is not JSON".into())
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
#[ignore = "needs an X display (DISPLAY) for pixelflux to shoot"]
async fn a_screenshot_is_a_png_on_the_audit_trail() {
    let root = scratch("selkies-screenshot-route");
    let recorder = Arc::new(Recorder::default());
    let server = state(&root, recorder.clone());
    let (status, headers, png) = get(&server, Who::Viewer, "/screenshot", &[]).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&png));
    assert_eq!(headers.get("content-type").unwrap(), "image/png");
    assert!(png.starts_with(b"\x89PNG"));
    let events = recorder.events.lock().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "screenshot");
    assert_eq!(events[0].1["display"], "primary");
    assert_eq!(events[0].1["role"], "viewer");
    assert_eq!(events[0].1["size_bytes"].as_u64(), Some(png.len() as u64));
    std::fs::remove_dir_all(&root).unwrap();
}
