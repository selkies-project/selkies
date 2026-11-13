/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One HTTP request against a server on the loopback address.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

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
