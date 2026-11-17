/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The image tier's helper inside the session under test, copied in as one static program.
//!
//! ```text
//! selkies-image-site serve ROOT [PORT]
//! selkies-image-site bw PORT BYTES
//! ```
//!
//! `serve` is the tester site, on loopback (8765 by default). Pages in `ROOT/site` are opened by a
//! browser on the session's own desktop; each POSTs what it saw to `/report/<name>`, which lands in
//! `ROOT/out/<name>.json` for the harness to read through the target's shell. Every page polls
//! `/next/<channel>` and follows it, so the harness moves a session browser from page to page by
//! writing `ROOT/next-<channel>`. A request only ever picks an entry out of a listing of what is
//! there (the site's files, the channel files) or out of the fixed set of report names; no path is
//! built from it.
//!
//! `bw` is a TCP endpoint on every address for two connections, to measure the link to the
//! session's host: a connection that sends `d` is sent `BYTES` bytes; one that sends `u` has
//! `BYTES` read from it and is answered with the count it got.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What the pages report, each to a file of its own.
const REPORTS: [&str; 9] = [
    "clip", "gamepad", "ime", "media", "pattern", "play", "pointer", "texture", "tone",
];
/// The largest report a page sends.
const MAX_BODY: usize = 16 << 20;

/// The regular files in `dir` named `prefix`..., by the rest of their name.
fn listing(dir: &Path, prefix: &str) -> HashMap<String, PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let rest = name.strip_prefix(prefix)?.to_string();
            Some((rest, e.path()))
        })
        .collect()
}

fn kind(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("js") => "text/javascript",
        Some("png") => "image/png",
        Some("webm") => "video/webm",
        Some("ogg") => "audio/ogg",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn reply(stream: &mut TcpStream, code: u16, body: &[u8], kind: &str) {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.0 {code} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

/// One request: its method, its path without the query, and its body.
fn request(stream: &TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }
    if length > MAX_BODY {
        return None;
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some((method, path, body))
}

fn handle(mut stream: TcpStream, root: &Path, counter: u64) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Some((method, path, body)) = request(&stream) else {
        reply(&mut stream, 400, b"", "text/plain");
        return;
    };
    if method == "GET" {
        if let Some(channel) = path.strip_prefix("/next/") {
            let body = listing(root, "next-")
                .get(channel)
                .and_then(|p| fs::read(p).ok())
                .map(|b| String::from_utf8_lossy(&b).trim().as_bytes().to_vec())
                .unwrap_or_default();
            reply(&mut stream, 200, &body, "text/plain");
            return;
        }
        let name = match path.trim_start_matches('/') {
            "" => "pattern.html",
            name => name,
        };
        match listing(&root.join("site"), "").get(name) {
            Some(full) => match fs::read(full) {
                Ok(data) => reply(&mut stream, 200, &data, kind(full)),
                Err(_) => reply(&mut stream, 404, b"", "text/plain"),
            },
            None => reply(&mut stream, 404, b"", "text/plain"),
        }
    } else if method == "POST" {
        let Some(name) = path
            .strip_prefix("/report/")
            .and_then(|n| REPORTS.iter().find(|r| **r == n))
        else {
            reply(&mut stream, 404, b"", "text/plain");
            return;
        };
        if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
            reply(&mut stream, 400, b"", "text/plain");
            return;
        }
        let out = root.join("out");
        let dest = out.join(format!("{name}.json"));
        let tmp = out.join(format!("{name}.json.{}.{counter}.tmp", std::process::id()));
        let written = fs::create_dir_all(&out)
            .and_then(|()| fs::write(&tmp, &body))
            .and_then(|()| fs::rename(&tmp, &dest));
        match written {
            Ok(()) => reply(&mut stream, 204, b"", "text/plain"),
            Err(_) => reply(&mut stream, 500, b"", "text/plain"),
        }
    } else {
        reply(&mut stream, 404, b"", "text/plain");
    }
}

fn serve(root: &str, port: u16) -> Result<(), String> {
    let root = fs::canonicalize(root).map_err(|e| format!("{root}: {e}"))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    for (counter, stream) in (0u64..).zip(listener.incoming()) {
        let Ok(stream) = stream else { continue };
        let root = root.clone();
        std::thread::spawn(move || handle(stream, &root, counter));
    }
    Ok(())
}

fn bw(port: u16, bytes: u64) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    for _ in 0..2 {
        let (mut c, _) = listener.accept().map_err(|e| e.to_string())?;
        let mut mode = [0u8; 1];
        c.read_exact(&mut mode).map_err(|e| e.to_string())?;
        if mode[0] == b'd' {
            let block = vec![0u8; 1 << 20];
            let mut left = bytes;
            while left > 0 {
                let n = usize::try_from(left.min(1 << 20)).unwrap_or(1 << 20);
                if c.write_all(&block[..n]).is_err() {
                    break;
                }
                left -= n as u64;
            }
        } else {
            let mut got = 0u64;
            let mut buf = vec![0u8; 1 << 20];
            while got < bytes {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n as u64,
                }
            }
            let _ = c.write_all(got.to_string().as_bytes());
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: selkies-image-site serve ROOT [PORT] | bw PORT BYTES";
    let number = |i: usize| -> Result<u64, String> {
        args.get(i)
            .ok_or(usage)?
            .parse()
            .map_err(|e| format!("{e}"))
    };
    match args.get(1).map(String::as_str) {
        Some("serve") => {
            let port = if args.len() > 3 { number(3)? } else { 8765 };
            serve(
                args.get(2).ok_or(usage)?,
                u16::try_from(port).map_err(|e| e.to_string())?,
            )
        }
        Some("bw") => bw(
            u16::try_from(number(2)?).map_err(|e| e.to_string())?,
            number(3)?,
        ),
        _ => Err(usage.to_string()),
    }
}
