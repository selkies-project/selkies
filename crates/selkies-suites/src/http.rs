/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One HTTP request against a server on the loopback address, and a listener that records what a
//! server posts to it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response as Answer};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A response: its status and its whole body.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    /// The body as text, lossily.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send `method` on `path` to the server on `port` with `headers` and `body`, over a connection of
/// its own, and read the whole response.
///
/// `path` goes on the request line as given, so an escape the test wrote reaches the server as
/// written. An error is a refused connection, a broken exchange, or no answer within `timeout`.
pub async fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    let exchange = async {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("handshake: {e}"))?;
        let driver = tokio::spawn(connection);
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", format!("127.0.0.1:{port}"))
            .header("Connection", "close");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|e| format!("request: {e}"))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| format!("send: {e}"))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("body: {e}"))?
            .to_bytes()
            .to_vec();
        driver.abort();
        Ok(Response { status, body })
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => Err(format!("no answer within {timeout:?}")),
    }
}

/// `GET path` from the server on `port`, bounded at ten seconds.
pub async fn get(port: u16, path: &str) -> Result<Response, String> {
    request(port, "GET", path, &[], &[], Duration::from_secs(10)).await
}

/// A request a [`Collector`] received: its headers, names in lower case, and its body.
#[derive(Clone)]
pub struct Posted {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Posted {
    /// The value of the header `name`, given in lower case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// An HTTP listener on the loopback address that records every request and answers 204: the
/// stand-in for an operator's collector. While stalled it holds every answer open, as a collector
/// that has stopped responding does.
pub struct Collector {
    port: u16,
    received: Arc<Mutex<Vec<Posted>>>,
    stalled: Arc<AtomicBool>,
    accepting: JoinHandle<()>,
}

impl Collector {
    /// Listen on a free port.
    ///
    /// # Panics
    ///
    /// When no loopback port can be bound.
    pub async fn start() -> Collector {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("a loopback port for the collector");
        let port = listener.local_addr().expect("the collector's port").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let stalled = Arc::new(AtomicBool::new(false));
        let accepting = tokio::spawn({
            let received = Arc::clone(&received);
            let stalled = Arc::clone(&stalled);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let received = Arc::clone(&received);
                    let stalled = Arc::clone(&stalled);
                    let record = service_fn(move |request: Request<Incoming>| {
                        let received = Arc::clone(&received);
                        let stalled = Arc::clone(&stalled);
                        async move {
                            let headers = request
                                .headers()
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.as_str().to_owned(),
                                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                                    )
                                })
                                .collect();
                            let body = request.into_body().collect().await?.to_bytes().to_vec();
                            while stalled.load(Ordering::Relaxed) {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                            received
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(Posted { headers, body });
                            let mut answer = Answer::new(Full::new(Bytes::new()));
                            *answer.status_mut() = hyper::StatusCode::NO_CONTENT;
                            Ok::<_, hyper::Error>(answer)
                        }
                    });
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), record)
                            .await;
                    });
                }
            }
        });
        Collector {
            port,
            received,
            stalled,
            accepting,
        }
    }

    /// The address of `path` on the collector.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Hold every answer open from now on, or let them go again.
    pub fn stall(&self, stalled: bool) {
        self.stalled.store(stalled, Ordering::Relaxed);
    }

    /// What has been received so far.
    pub fn received(&self) -> Vec<Posted> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What has been received once `enough` accepts it, or once `timeout` has passed.
    pub async fn received_when(
        &self,
        timeout: Duration,
        enough: impl Fn(&[Posted]) -> bool,
    ) -> Vec<Posted> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let received = self.received();
            if enough(&received) || tokio::time::Instant::now() >= deadline {
                return received;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stalled.store(false, Ordering::Relaxed);
        self.accepting.abort();
    }
}
