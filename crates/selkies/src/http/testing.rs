/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared fixtures of the control plane's tests: settings parsed from a given command line,
//! a server on an ephemeral port driven by a real HTTP client, and a capture of the log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use super::listen::{self, ListenConfig, Listener};
use crate::settings::{self, Settings};

/// Settings from `args` and `env` alone, the shell's environment kept out.
pub(crate) fn parse_settings(args: &[&str], env: &[(&str, &str)]) -> Settings {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let map: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Settings::parse(&args, &|k| map.get(k).cloned())
}

/// Install default settings for a test under [`rerun_alone`] that reads the process settings.
pub(crate) fn ensure_settings() {
    settings::install(parse_settings(&[], &[]));
}

/// Run the named test by itself in a child process, for a test that touches what a process
/// has one of: the global logger, which another module's tests may claim first and may refuse
/// to share, the environment, the settings other modules' tests read, or a socket it closes
/// and needs gone at once, which a test forking beside it holds until its exec. True in the parent,
/// which has asserted the child's result and has nothing left to do; false in the child, where
/// the test goes on.
pub(crate) fn rerun_alone(test: &str) -> bool {
    const CHILD: &str = "SELKIES_HTTP_TEST_ALONE";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let exe = std::env::current_exe().expect("the test binary");
    let output = std::process::Command::new(exe)
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("the test binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{test} alone: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

/// A log sink collecting `LEVEL message` lines of every target, for a test running under
/// [`rerun_alone`].
pub(crate) struct Capture {
    lines: Lines,
}

/// `(target, "LEVEL message")` per record.
type Lines = Arc<Mutex<Vec<(String, String)>>>;

static CAPTURE: OnceLock<Arc<Mutex<Option<Lines>>>> = OnceLock::new();

struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let slot = CAPTURE
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .clone();
        if let Some(lines) = slot {
            let level = match record.level() {
                log::Level::Error => "ERROR",
                log::Level::Warn => "WARNING",
                log::Level::Info => "INFO",
                log::Level::Debug => "DEBUG",
                log::Level::Trace => "TRACE",
            };
            lines.lock().unwrap().push((
                record.target().to_string(),
                format!("{level} {}", record.args()),
            ));
        }
    }

    fn flush(&self) {}
}

impl Capture {
    /// Start collecting the log.
    pub(crate) fn install() -> Capture {
        static LOGGER: CaptureLogger = CaptureLogger;
        log::set_logger(&LOGGER).expect("rerun_alone leaves this process's logger free");
        log::set_max_level(log::LevelFilter::Debug);
        let lines = Arc::new(Mutex::new(Vec::new()));
        *CAPTURE.get_or_init(Default::default).lock().unwrap() = Some(lines.clone());
        Capture { lines }
    }

    /// The captured lines of one target, which tests running beside this one do not write to.
    pub(crate) fn lines_for(&self, target: &str) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == target)
            .map(|(_, line)| line.clone())
            .collect()
    }

    pub(crate) fn clear(&self) {
        self.lines.lock().unwrap().clear();
    }

    pub(crate) fn remove(self) {
        *CAPTURE.get_or_init(Default::default).lock().unwrap() = None;
    }
}

/// One answer of the server under test.
pub(crate) struct Reply {
    pub status: u16,
    pub headers: axum::http::HeaderMap,
    pub body: String,
}

impl Reply {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A server on an ephemeral loopback port and the client that drives it.
pub(crate) struct TestServer {
    listener: Option<Listener>,
    pub port: u16,
    /// The address the listener bound first.
    addr: std::net::SocketAddr,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl TestServer {
    /// Serve `router` on `127.0.0.1` with the default listen settings apart from the port.
    pub(crate) async fn start(router: Router) -> TestServer {
        TestServer::start_with(
            router,
            ListenConfig {
                addr: "127.0.0.1".into(),
                port: 0,
                ..ListenConfig::default()
            },
        )
        .await
    }

    /// Serve `router` with `cfg`; a port of 0 binds atomically on one address and takes
    /// [`free_port`] on several, which have to share it.
    pub(crate) async fn start_with(router: Router, mut cfg: ListenConfig) -> TestServer {
        if cfg.port == 0 && cfg.addr.contains(',') {
            cfg.port = free_port();
        }
        let listener = listen::start(router, &cfg, None).await.expect("listener");
        let addr = listener.local_addrs().first().copied().unwrap_or_else(|| {
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, cfg.port))
        });
        let client = Client::builder(TokioExecutor::new()).build_http();
        TestServer {
            listener: Some(listener),
            port: addr.port(),
            addr,
            client,
        }
    }

    /// A raw TCP connection to the listener, for a test that writes its request by hand.
    pub(crate) async fn connect(&self) -> tokio::net::TcpStream {
        tokio::net::TcpStream::connect(self.addr)
            .await
            .expect("the test server accepts")
    }

    pub(crate) fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// One request with `headers`; a body of one byte on every non-Upgrade request, as a
    /// client that always sends something.
    pub(crate) async fn call(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
        let body = if headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("upgrade"))
        {
            ""
        } else {
            "x"
        };
        self.call_body(method, path, headers, body.as_bytes()).await
    }

    /// One request with `headers` and `body`.
    pub(crate) async fn call_body(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.origin()));
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        let request = request
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        let response = self.client.request(request).await.expect("response");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    pub(crate) async fn stop(mut self) {
        if let Some(listener) = self.listener.take() {
            listener.stop().await;
        }
    }
}

/// A port for a check that has to name one before binding: nothing listens on it now, and it
/// sits below the kernel's ephemeral range, so neither another test's bind to port 0 nor any
/// outbound connection's source port can take it in the meantime.
pub(crate) fn free_port() -> u16 {
    loop {
        let port = rand::random_range(20_000..32_000);
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// A fresh scratch directory under the system temp dir.
pub(crate) fn tempfile_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "{prefix}{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `Authorization: Basic` for `user:password`.
pub(crate) fn basic(user: &str, password: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}
