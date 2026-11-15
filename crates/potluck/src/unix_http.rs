//! A one-shot HTTP/1.1 GET over a Unix socket, just enough for `health-check`.

use std::io;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Longest response `get` accepts, head and body; every admin answer it fetches is far
/// smaller, and an endless stream must end in an error, not in memory.
pub(crate) const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// One `GET target` over the Unix socket at `path` with `Connection: close`; returns the
/// status code and the body. Reads at most `MAX_RESPONSE_BYTES`, `InvalidData` past that.
pub(crate) async fn get(path: &Path, target: &str) -> io::Result<(u16, String)> {
    let mut stream = UnixStream::connect(path).await?;
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
