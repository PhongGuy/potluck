//! The tar writer: the staged database and repositories as one archive owned by the image user.

use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use tar::{Builder, EntryType, Header};

/// The container's non-root user and group, so a restore into the image needs no chown.
const IMAGE_ID: u64 = 65532;

/// Writes `staging/server.db`, then `staging/repos` recursively in name order, as a tar of
/// relative paths (directories end in `/`), GNU headers, uid and gid 65532, the file's
/// permission bits and mtime, no extended attributes; then the end-of-archive blocks.
/// Anything else in `staging` is left out; a symlink or special file under `repos` is
/// `ErrorKind::InvalidData` naming its path.
pub(crate) fn write_tar(staging: &Path, out: &mut dyn Write) -> io::Result<()> {
    let mut builder = Builder::new(out);
    append(
        &mut builder,
        &staging.join("server.db"),
        Path::new("server.db"),
    )?;
    append(&mut builder, &staging.join("repos"), Path::new("repos"))?;
    builder.into_inner()?;
    Ok(())
}

/// Appends `src` as `name`: a regular file with its bytes, or a directory followed by its
/// children in file-name order. Links and special files are refused rather than followed.
fn append(builder: &mut Builder<&mut dyn Write>, src: &Path, name: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_file() {
        let mut header = header(&meta, EntryType::Regular, meta.len());
        builder.append_data(&mut header, name, File::open(src)?)
    } else if meta.is_dir() {
        let mut dir = name.as_os_str().to_owned();
        dir.push("/");
        let mut header = header(&meta, EntryType::Directory, 0);
        builder.append_data(&mut header, dir, io::empty())?;
        let mut children = fs::read_dir(src)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            append(builder, &src.join(&child), &name.join(&child))?;
        }
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is neither a file nor a directory", src.display()),
        ))
    }
}

/// A GNU header owned by the image user, carrying the source's permission bits and mtime.
fn header(meta: &Metadata, kind: EntryType, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(meta.permissions().mode() & 0o7777);
    header.set_uid(IMAGE_ID);
    header.set_gid(IMAGE_ID);
    header.set_mtime(u64::try_from(meta.mtime()).unwrap_or(0));
    header
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    fn staging(dir: &Path) {
        std::fs::write(dir.join("server.db"), b"db").unwrap();
        std::fs::write(dir.join("extra"), b"not archived").unwrap();
        let repo = dir.join("repos/core/api.git");
        std::fs::create_dir_all(repo.join("objects/ab")).unwrap();
        std::fs::write(repo.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        let obj = repo.join("objects/ab/cdef");
        std::fs::write(&obj, b"blob").unwrap();
        std::fs::set_permissions(&obj, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    #[test]
    fn the_tar_holds_the_database_then_the_repositories_owned_by_65532() {
        let dir = tempfile::tempdir().unwrap();
        staging(dir.path());
        let mut out = Vec::new();
        write_tar(dir.path(), &mut out).unwrap();
        let mut archive = tar::Archive::new(out.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let header = entry.header();
            assert_eq!(
                (header.uid().unwrap(), header.gid().unwrap()),
                (65532, 65532),
                "{path} owner"
            );
            let mode = header.mode().unwrap();
            let mut body = Vec::new();
            entry.read_to_end(&mut body).unwrap();
            seen.push((path, mode & 0o7777, body));
        }
        let paths: Vec<&str> = seen.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "server.db",
                "repos/",
                "repos/core/",
                "repos/core/api.git/",
                "repos/core/api.git/HEAD",
                "repos/core/api.git/objects/",
                "repos/core/api.git/objects/ab/",
                "repos/core/api.git/objects/ab/cdef"
            ],
            "order and contents"
        );
        assert_eq!(seen[0].2, b"db", "database bytes");
        assert_eq!(seen[7].1, 0o444, "object mode kept");
        assert_eq!(seen[7].2, b"blob", "object bytes");
    }

    #[test]
    fn a_symlink_under_repos_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        staging(dir.path());
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("repos/core/api.git/link"))
            .unwrap();
        let err = write_tar(dir.path(), &mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("link"), "names the path: {err}");
    }
}
