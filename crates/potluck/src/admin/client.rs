//! HTTP/1.1 to the running server over `<data>/admin.sock`.

use std::io;
use std::path::Path;
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Longest body `send` and `get_once` accept; every admin answer is far smaller, and an endless
/// stream must end in an error, not in memory.
pub(crate) const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// The methods the admin API uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verb {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
}

impl Verb {
    fn method(self) -> reqwest::Method {
        match self {
            Verb::Get => reqwest::Method::GET,
            Verb::Post => reqwest::Method::POST,
            Verb::Put => reqwest::Method::PUT,
            Verb::Patch => reqwest::Method::PATCH,
            Verb::Delete => reqwest::Method::DELETE,
        }
    }
}

/// A client bound to one socket. Clone is cheap (reqwest pools inside).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AdminClient {
    http: reqwest::Client,
}

impl AdminClient {
    /// Connects once to `socket` and drops that connection, so a missing file is `NotFound`
    /// and a dead socket `ConnectionRefused`, then builds the client with a 60 s timeout.
    pub async fn connect(socket: &Path) -> io::Result<Self> {
        drop(UnixStream::connect(socket).await?);
        let http = reqwest::Client::builder()
            .unix_socket(socket)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self { http })
    }

    /// One request to `http://potluck{target}` over the socket; `json` is sent as the body with
    /// `Content-Type: application/json`. The body is read chunk by chunk and refused past
    /// `MAX_RESPONSE_BYTES` with `InvalidData` "response longer than 64 KiB". Every status is
    /// `Ok`; only transport failures are `Err`.
    pub async fn send(
        &self,
        verb: Verb,
        target: &str,
        json: Option<&str>,
    ) -> io::Result<ApiResponse> {
        let mut request = self
            .http
            .request(verb.method(), format!("http://potluck{target}"));
        if let Some(json) = json {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(json.to_owned());
        }
        let mut response = request.send().await.map_err(io::Error::other)?;
        let status = response.status().as_u16();
        // reqwest reads bodies without a limit, so the cap is enforced here, chunk by chunk.
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
            body.extend_from_slice(&chunk);
            if body.len() as u64 > MAX_RESPONSE_BYTES {
                return Err(invalid("response longer than 64 KiB"));
            }
        }
        let text = String::from_utf8(body).map_err(|_| invalid("response is not UTF-8"))?;
        Ok(ApiResponse { status, text })
    }
}

/// One answer: the status and the body as text (empty for 204).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApiResponse {
    status: u16,
    text: String,
}

impl ApiResponse {
    /// The HTTP status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The body as the server sent it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The body parsed as JSON; `None` when empty or not JSON. Crate-private: serde_json
    /// stays out of the public surface.
    pub(crate) fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.text).ok()
    }
}

/// One `GET target` over its own single connection with `Connection: close`; returns the
/// status and the body, `InvalidData` past `MAX_RESPONSE_BYTES`. For `health-check`, which must
/// judge any peer: hyper refuses bytes a server sends before the request, this reader does not.
pub(crate) async fn get_once(socket: &Path, target: &str) -> io::Result<(u16, String)> {
    let mut stream = UnixStream::connect(socket).await?;
    let request = format!("GET {target} HTTP/1.1\r\nHost: potluck\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    // `Connection: close` makes the server end the stream after one response, so EOF
    // delimits it and the body is whatever follows the head (it carries Content-Length).
    let mut raw = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut raw)
        .await?;
    if raw.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(invalid("response longer than 64 KiB"));
    }
    let text = String::from_utf8(raw).map_err(|_| invalid("response is not UTF-8"))?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid("response has no end of head"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| invalid("malformed status line"))?;
    Ok((status, body.to_owned()))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
