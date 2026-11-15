//! Spools a smart HTTP POST body to an anonymous file before git is spawned.

use std::path::Path;

use axum::body::Body;

use crate::git::GitError;

/// Copies the request body to an anonymous file under `tmp_dir`, inflating
/// `Content-Encoding: gzip`, and refuses more than `limit` bytes of (inflated) data. The file is
/// unlinked at creation and returned positioned at 0.
pub(crate) async fn spool(
    body: Body,
    encoding: Option<&str>,
    limit: u64,
    tmp_dir: &Path,
) -> Result<std::fs::File, GitError> {
    use async_compression::tokio::bufread::GzipDecoder;
    use futures_util::TryStreamExt;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use tokio_util::io::StreamReader;

    let reader = StreamReader::new(body.into_data_stream().map_err(std::io::Error::other));
    let mut file =
        tokio::fs::File::from_std(tempfile::tempfile_in(tmp_dir).map_err(GitError::Spawn)?);
    // Read one byte past the cap: landing exactly on `limit + 1` is the proof of excess, and the
    // decoder sits inside the `take`, so a gzip bomb is measured inflated and cut there.
    let budget = limit.saturating_add(1);
    let copied = match encoding {
        None => tokio::io::copy(&mut reader.take(budget), &mut file).await,
        Some(enc) if enc.eq_ignore_ascii_case("gzip") => {
            tokio::io::copy(&mut GzipDecoder::new(reader).take(budget), &mut file).await
        }
        Some(other) => return Err(GitError::BadEncoding(other.to_owned())),
    }
    .map_err(GitError::Body)?;
    if copied > limit {
        return Err(GitError::TooLarge { limit });
    }
    file.flush().await.map_err(GitError::Spawn)?;
    file.rewind().await.map_err(GitError::Spawn)?;
    Ok(file.into_std().await)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::Path;

    use axum::body::Body;
    use tokio::io::AsyncWriteExt;

    use super::*;

    /// `spool` into a fresh temp dir.
    async fn spool_in(
        body: Body,
        encoding: Option<&str>,
        limit: u64,
    ) -> Result<std::fs::File, GitError> {
        let dir = tempfile::tempdir().unwrap();
        spool(body, encoding, limit, dir.path()).await
    }

    async fn gzipped(data: &[u8]) -> Vec<u8> {
        let mut enc = async_compression::tokio::write::GzipEncoder::new(Vec::new());
        enc.write_all(data).await.unwrap();
        enc.shutdown().await.unwrap();
        enc.into_inner()
    }

    fn contents(mut file: std::fs::File) -> Vec<u8> {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        buf
    }

    #[tokio::test]
    async fn an_identity_body_is_copied_and_rewound_and_leaves_no_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let file = spool(
            Body::from("0014command=ls-refs\n0000"),
            None,
            1024,
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(contents(file), b"0014command=ls-refs\n0000");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "the spool is anonymous: nothing is left in tmp"
        );
        let missing = Path::new("/nonexistent/potluck-tmp");
        let err = spool(Body::from("x"), None, 1024, missing)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::Spawn(_)),
            "a missing tmp dir is a server-side failure: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_gzip_body_is_inflated() {
        let body = gzipped(b"hello pack").await;
        let file = spool_in(Body::from(body), Some("gzip"), 1024)
            .await
            .unwrap();
        assert_eq!(contents(file), b"hello pack");
        let body = gzipped(b"case").await;
        spool_in(Body::from(body), Some("GZIP"), 1024)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exactly_the_limit_passes_and_one_more_byte_is_too_large() {
        spool_in(Body::from(vec![7u8; 10]), None, 10).await.unwrap();
        let err = spool_in(Body::from(vec![7u8; 11]), None, 10)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::TooLarge { limit: 10 }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn the_cap_measures_inflated_bytes() {
        let bomb = gzipped(&vec![0u8; 1024 * 1024]).await;
        assert!(
            bomb.len() < 4096,
            "a megabyte of zeros compresses to a few KiB: {}",
            bomb.len()
        );
        let err = spool_in(Body::from(bomb), Some("gzip"), 4096)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::TooLarge { limit: 4096 }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn other_encodings_and_corrupt_gzip_are_body_errors() {
        let err = spool_in(Body::from("x"), Some("br"), 1024)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::BadEncoding(ref e) if e == "br"),
            "got {err:?}"
        );
        let err = spool_in(Body::from("not gzip at all"), Some("gzip"), 1024)
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Body(_)), "got {err:?}");
    }
}
