//! A one-shot HTTP/1.1 GET over a Unix socket, just enough for `health-check`.

use std::io;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// One `GET target` over the Unix socket at `path` with `Connection: close`; returns the
/// status code and the body. GET only, no chunked decoding: enough for `health-check`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired by health-check, which follows")
)]
pub(crate) async fn get(path: &Path, target: &str) -> io::Result<(u16, String)> {
    let mut stream = UnixStream::connect(path).await?;
    let request = format!("GET {target} HTTP/1.1\r\nHost: potluck\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    // `Connection: close` makes the server end the stream after one response, so EOF
    // delimits it and the body is whatever follows the head (it carries Content-Length).
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
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
