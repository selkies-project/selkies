/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A download addons/base/fetch-verified hands on is the one its source's hash names.
//!
//! The helper lets a build take whichever version is current (the newest release, a branch's
//! head, a vendor's key file) and still refuse what does not match: a release asset against the
//! SHA-256 GitHub records for it, a file at a commit against the blob hash GitHub's API names,
//! an OpenPGP key against the fingerprints it is meant to carry. The helper runs as the images
//! run it, with the network replaced by a proxy that answers for GitHub and the key's host under
//! a certificate authority made for the run, so each case is the comparison alone: what matches
//! is written and what does not is removed with the command failing. The key case parses
//! Mozilla's apt signing key, whose fingerprint Mozilla publishes, so the parser is held to a
//! number it did not compute itself.
//!
//! Also held here are the places that rely on it: the base image installs it, and the desktop
//! image and the AppImage build go through it.
//!
//! The helper is a POSIX shell script on curl, jq and the coreutils, run here under `sh`; these
//! run with `cargo test --workspace`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, repo, workdir};

/// packages.mozilla.org/apt/repo-signing-key.gpg, fingerprint
/// 35BA A0B3 3E9E B396 F59C A838 C0BA 5CE6 DC63 15A3 as Mozilla documents it
const MOZILLA_KEY: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----

xsBNBGCRt7MBCADkYJHHQQoL6tKrW/LbmfR9ljz7ib2aWno4JO3VKQvLwjyUMPpq
/SXXMOnx8jXwgWizpPxQYDRJ0SQXS9ULJ1hXRL/OgMnZAYvYDeV2jBnKsAIEdiG/
e1qm8P4W9qpWJc+hNq7FOT13RzGWRx57SdLWSXo0KeY38r9lvjjOmT/cuOcmjwlD
T9XYf/RSO+yJ/AsyMdAr+ZbDeQUd9HYJiPdI04lGaGM02MjDMnx+monc+y54t+Z+
ry1WtQdzoQt9dHlIPlV1tR+xV5DHHsejCZxu9TWzzSlL5wfBBeEz7R/OIzivGJpW
QdJzd+2QDXSRg9q2XYWP5ZVtSgjVVJjNlb6ZABEBAAHNVEFydGlmYWN0IFJlZ2lz
dHJ5IFJlcG9zaXRvcnkgU2lnbmVyIDxhcnRpZmFjdC1yZWdpc3RyeS1yZXBvc2l0
b3J5LXNpZ25lckBnb29nbGUuY29tPsLAjgQTAQoAOBYhBDW6oLM+nrOW9ZyoOMC6
XObcYxWjBQJgkbezAhsDBQsJCAcCBhUKCQgLAgQWAgMBAh4BAheAAAoJEMC6XObc
YxWj+igIAMFh6DrAYMeq9sbZ1ZG6oAMrinUheGQbEqe76nIDQNsZnhDwZ2wWqgVC
7DgOMqlhQmOmzm7M6Nzmq2dvPwq3xC2OeI9fQyzjT72deBTzLP7PJok9PJFOMdLf
ILSsUnmMsheQt4DUO0jYAX2KUuWOIXXJaZ319QyoRNBPYa5qz7qXS7wHLOY89IDq
fHt6Aud8ER5zhyOyhytcYMeaGC1g1IKWmgewnhEq02FantMJGlmmFi2eA0EPD02G
C3742QGqRxLwjWsm5/TpyuU24EYKRGCRm7QdVIo3ugFSetKrn0byOxWGBvtu4fH8
XWvZkRT+u+yzH1s5yFYBqc2JTrrJvRU=
=QnvN
-----END PGP PUBLIC KEY BLOCK-----
";
const MOZILLA_FINGERPRINT: &str = "35BAA0B33E9EB396F59CA838C0BA5CE6DC6315A3";

/// The hosts the proxy answers for.
const HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "raw.githubusercontent.com",
    "example.invalid",
];

type Responses = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A proxy that ends TLS for [`HOSTS`] and answers each `https://` URL from its table, 404 for
/// any other, and the certificate authority a client trusts it by.
struct Network {
    port: u16,
    ca: PathBuf,
    responses: Responses,
}

impl Network {
    fn start(dir: &Path) -> Network {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
        let ca_key = KeyPair::generate().expect("a CA key");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA parameters");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "fetch-verified test CA");
        let ca_cert = ca_params.self_signed(&ca_key).expect("the CA");
        let issuer = Issuer::new(ca_params, ca_key);
        let leaf_key = KeyPair::generate().expect("a host key");
        let mut leaf_params =
            CertificateParams::new(HOSTS.map(str::to_owned).to_vec()).expect("host parameters");
        leaf_params.use_authority_key_identifier_extension = true;
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = leaf_params
            .signed_by(&leaf_key, &issuer)
            .expect("the host certificate");
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, ca_cert.pem()).expect("the CA file");

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone(), ca_cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
            )
            .expect("the TLS configuration");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("a proxy port");
        let port = listener.local_addr().expect("its address").port();
        let responses: Responses = Arc::default();
        let table = Arc::clone(&responses);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let config = Arc::clone(&config);
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    let _ = tunnel(stream, &config, &table);
                });
            }
        });
        Network {
            port,
            ca,
            responses,
        }
    }

    fn serve(&self, responses: HashMap<String, Vec<u8>>) {
        *self.responses.lock().expect("the table") = responses;
    }
}

/// Reads an HTTP head: its request line, the rest discarded.
fn head(reader: &mut impl BufRead) -> std::io::Result<String> {
    let mut first = String::new();
    reader.read_line(&mut first)?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    Ok(first.trim_end().to_owned())
}

/// One proxied connection: the CONNECT, the TLS handshake for its host, one request answered.
fn tunnel(
    stream: TcpStream,
    config: &Arc<rustls::ServerConfig>,
    table: &Responses,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let connect = head(&mut reader)?;
    let host = connect
        .split_whitespace()
        .nth(1)
        .and_then(|target| target.rsplit_once(':'))
        .map(|(host, _)| host.to_owned())
        .unwrap_or_default();
    let mut raw = stream;
    raw.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    let connection =
        rustls::ServerConnection::new(Arc::clone(config)).map_err(std::io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(connection, raw);
    let mut request = BufReader::new(&mut tls);
    let line = head(&mut request)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let url = format!("https://{host}{path}");
    let found = table.lock().expect("the table").get(&url).cloned();
    let (status, body) = match found {
        Some(body) => ("200 OK", body),
        None => ("404 Not Found", b"not found".to_vec()),
    };
    write!(
        tls,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    tls.write_all(&body)?;
    tls.conn.send_close_notify();
    tls.flush()
}

/// What a hashing tool of coreutils prints for `data`.
fn digest(tool: &str, data: &[u8]) -> String {
    let mut child = Command::new(tool)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(data)
        .expect("the data");
    let out = child.wait_with_output().expect("the digest");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The helper run on `args` through `network`: its error (None where it succeeded) and what it
/// printed.
fn run(
    network: &Network,
    responses: HashMap<String, Vec<u8>>,
    args: &[&str],
) -> (Option<String>, String) {
    network.serve(responses);
    let mut command = Command::new("sh");
    command
        .arg(repo().join("addons/base/fetch-verified"))
        .args(args)
        .env("https_proxy", format!("http://127.0.0.1:{}", network.port))
        .env("SSL_CERT_FILE", &network.ca)
        .env_remove("HTTPS_PROXY")
        .env_remove("no_proxy")
        .env_remove("NO_PROXY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN");
    match run_within(&mut command, Duration::from_secs(60)) {
        Ok(ran) if ran.status.success() => (None, ran.stdout),
        Ok(ran) => (Some(ran.stderr.trim().to_owned()), ran.stdout),
        Err(e) => (Some(e), String::new()),
    }
}

fn release_responses(body: &[u8], digest: Option<&str>) -> HashMap<String, Vec<u8>> {
    let api = "https://api.github.com/repos/o/r/releases";
    let url = "https://github.com/o/r/releases/download/v1.2.3/tool-1.2.3.tar.gz";
    let info = serde_json::json!({
        "tag_name": "v1.2.3",
        "assets": [{"name": "tool-1.2.3.tar.gz", "digest": digest, "browser_download_url": url}],
    })
    .to_string()
    .into_bytes();
    HashMap::from([
        (format!("{api}/latest"), info.clone()),
        (format!("{api}/tags/v1.2.3"), info),
        (url.to_owned(), body.to_vec()),
    ])
}

fn file_responses(body: &[u8], served: &[u8]) -> HashMap<String, Vec<u8>> {
    let commit = "c".repeat(40);
    let mut object = format!("blob {}\0", body.len()).into_bytes();
    object.extend_from_slice(body);
    let blob = digest("sha1sum", &object);
    let api = "https://api.github.com/repos/o/r";
    HashMap::from([
        (
            format!("{api}/commits/main"),
            serde_json::json!({"sha": commit}).to_string().into_bytes(),
        ),
        (
            format!("{api}/contents/dir/f.sh?ref={commit}"),
            serde_json::json!({"sha": blob}).to_string().into_bytes(),
        ),
        (
            format!("https://raw.githubusercontent.com/o/r/{commit}/dir/f.sh"),
            served.to_vec(),
        ),
    ])
}

fn shown(err: Option<&String>, printed: &str) -> String {
    err.map_or_else(|| printed.trim().to_owned(), |e| e.clone())
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

#[test]
fn a_download_is_the_one_its_source_names() {
    let mut checks = Checks::new("fetch-verified");
    let dir = workdir().join(format!("fetch-verified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    let network = Network::start(&dir);
    let out = dir.join("asset");
    let out_arg = out.to_str().expect("a UTF-8 path");

    let body = b"release asset\n";
    let good = format!("sha256:{}", digest("sha256sum", body));
    let (err, printed) = run(
        &network,
        release_responses(body, Some(&good)),
        &["release", "o/r", "latest", "tool-{version}.tar.gz", out_arg],
    );
    checks.check(
        "a release asset that hashes to its digest is written, and its tag printed",
        err.is_none() && printed.trim() == "v1.2.3" && read(&out) == body,
        shown(err.as_ref(), &printed),
    );
    let (err, _) = run(
        &network,
        release_responses(body, Some(&good)),
        &["release", "o/r", "v1.2.3", "tool-1.2.3.tar.gz", out_arg],
    );
    checks.check(
        "a release named by its tag is looked up by that tag",
        err.is_none(),
        err.unwrap_or_default(),
    );

    std::fs::write(&out, b"stale").expect("a stale file");
    let (err, _) = run(
        &network,
        release_responses(b"release asset\nx", Some(&good)),
        &["release", "o/r", "latest", "tool-{version}.tar.gz", out_arg],
    );
    checks.check(
        "an asset that does not hash to its digest fails and leaves no file",
        err.as_ref().is_some_and(|e| e.contains("hashes to")) && !out.exists(),
        err.unwrap_or_default(),
    );
    let (err, _) = run(
        &network,
        release_responses(body, None),
        &["release", "o/r", "latest", "tool-{version}.tar.gz", out_arg],
    );
    checks.check(
        "an asset with no recorded digest is refused",
        err.as_ref().is_some_and(|e| e.contains("no SHA-256")) && !out.exists(),
        err.unwrap_or_default(),
    );
    let (err, _) = run(
        &network,
        release_responses(body, Some(&good)),
        &["release", "o/r", "latest", "other.tar.gz", out_arg],
    );
    checks.check(
        "an asset the release does not carry is refused",
        err.as_ref().is_some_and(|e| e.contains("has no asset")),
        err.unwrap_or_default(),
    );

    let script = b"#!/bin/sh\necho verified\n";
    let (err, printed) = run(
        &network,
        file_responses(script, script),
        &["file", "o/r", "main", "dir/f.sh", out_arg],
    );
    checks.check(
        "a file at a ref that matches its blob hash is written, and the commit printed",
        err.is_none() && printed.trim() == "c".repeat(40) && read(&out) == script,
        shown(err.as_ref(), &printed),
    );
    let (err, _) = run(
        &network,
        file_responses(script, b"#!/bin/sh\necho verified\n# appended\n"),
        &["file", "o/r", "main", "dir/f.sh", out_arg],
    );
    checks.check(
        "a file whose blob hash differs fails and leaves no file",
        err.as_ref().is_some_and(|e| e.contains("hashes to blob")) && !out.exists(),
        err.unwrap_or_default(),
    );

    let url = "https://example.invalid/key.asc";
    let key = || HashMap::from([(url.to_owned(), MOZILLA_KEY.as_bytes().to_vec())]);
    let (err, _) = run(&network, key(), &["key", url, MOZILLA_FINGERPRINT, out_arg]);
    checks.check(
        "Mozilla's key parses to the fingerprint Mozilla publishes",
        err.is_none() && read(&out) == MOZILLA_KEY.as_bytes(),
        err.unwrap_or_default(),
    );
    let spaced = MOZILLA_FINGERPRINT
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let (err, _) = run(
        &network,
        key(),
        &["key", url, &format!("{},{spaced}", "0".repeat(40)), out_arg],
    );
    checks.check(
        "a fingerprint is matched spaced, lower-case, and among others",
        err.is_none(),
        err.unwrap_or_default(),
    );
    let (err, _) = run(&network, key(), &["key", url, &"0".repeat(40), out_arg]);
    checks.check(
        "a key that is not the one named fails and leaves no file",
        err.as_ref()
            .is_some_and(|e| e.contains(MOZILLA_FINGERPRINT))
            && !out.exists(),
        err.unwrap_or_default(),
    );
    let (err, _) = run(
        &network,
        HashMap::from([(url.to_owned(), b"<html>not a key</html>".to_vec())]),
        &["key", url, MOZILLA_FINGERPRINT, out_arg],
    );
    checks.check(
        "a page that is no key is refused",
        err.is_some() && !out.exists(),
        err.unwrap_or_default(),
    );

    let (err, _) = run(
        &network,
        HashMap::new(),
        &["file", "o/r", "main", "dir/f.sh"],
    );
    checks.check(
        "a path without its output is a usage error",
        err.as_ref()
            .is_some_and(|e| e.contains("fetch-verified release")),
        err.unwrap_or_default().lines().next().unwrap_or_default(),
    );

    let text = |path: &str| std::fs::read_to_string(repo().join(path)).unwrap_or_default();
    let base = text("addons/base/Dockerfile");
    checks.check(
        "the base image installs it",
        base.contains("COPY addons/base/fetch-verified /usr/local/bin/fetch-verified"),
        "addons/base/Dockerfile",
    );
    let desktop = text("addons/desktop/Dockerfile");
    checks.check(
        "the desktop image checks its downloads with it",
        desktop.matches("fetch-verified key ").count() == 2
            && desktop.contains("fetch-verified release linuxserver/proot-apps")
            && !desktop.contains("dl.google.com/linux/direct"),
        "addons/desktop/Dockerfile",
    );
    let appimage = text("infra/appimage/build.sh");
    checks.check(
        "the AppImage build fetches its runtime with it",
        appimage.contains("addons/base/fetch-verified\" release AppImage/type2-runtime"),
        "infra/appimage/build.sh",
    );
    let _ = std::fs::remove_dir_all(&dir);
    checks.finish("");
}
