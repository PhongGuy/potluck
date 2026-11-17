//! The git plumbing the hook runs inside the push's quarantine.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

/// A hex object name: 40 (SHA-1) or 64 (SHA-256) lower-case hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Sha(String);

impl Sha {
    /// Parses an object name of either width, folding upper-case hex to lower; anything else
    /// is [`ParseError::NotHex`].
    pub fn parse(hex: &str) -> Result<Sha, ParseError> {
        let width_ok = hex.len() == 40 || hex.len() == 64;
        if !width_ok || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ParseError::NotHex {
                text: hex.to_owned(),
            });
        }
        Ok(Sha(hex.to_ascii_lowercase()))
    }

    /// [`Sha::parse`] on a raw field; a field that is not UTF-8 is [`ParseError::NotHex`].
    fn parse_bytes(field: &[u8]) -> Result<Sha, ParseError> {
        let text = std::str::from_utf8(field).map_err(|_| ParseError::NotHex {
            text: String::from_utf8_lossy(field).into_owned(),
        })?;
        Sha::parse(text)
    }

    /// The lower-case hex digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Every digit is `0`: git's "no object".
    pub fn is_zero(&self) -> bool {
        self.0.bytes().all(|b| b == b'0')
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `<old> <new> <ref>` line of pre-receive stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefUpdate {
    old: Sha,
    new: Sha,
    name: Vec<u8>,
}

impl RefUpdate {
    /// Parses exactly three space-separated fields; a trailing newline is fine. The ref name
    /// keeps its exact bytes, while the object names must be UTF-8 hex.
    pub fn parse_line(line: &[u8]) -> Result<RefUpdate, ParseError> {
        let shape = || ParseError::Shape {
            line: String::from_utf8_lossy(line).into_owned(),
        };
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let fields: Vec<&[u8]> = trimmed.split(|&b| b == b' ').collect();
        let [old, new, name] = fields.as_slice() else {
            return Err(shape());
        };
        if name.is_empty() {
            return Err(shape());
        }
        Ok(RefUpdate {
            old: Sha::parse_bytes(old)?,
            new: Sha::parse_bytes(new)?,
            name: name.to_vec(),
        })
    }

    /// The ref's value before the push.
    pub fn old(&self) -> &Sha {
        &self.old
    }

    /// The ref's value the push proposes.
    // An accessor named after git's own `<old> <new> <ref>` field, not a constructor.
    #[allow(clippy::new_ret_no_self, clippy::wrong_self_convention)]
    pub fn new(&self) -> &Sha {
        &self.new
    }

    /// The full ref name as git sent it, such as `refs/heads/main`; not always UTF-8.
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// `new` is all zeros: the ref is being deleted and there is nothing to scan.
    pub fn is_delete(&self) -> bool {
        self.new.is_zero()
    }
}

/// Why a stdin line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// Not 40 or 64 hex digits.
    #[error("not an object name: {text:?}")]
    NotHex {
        /// The offending field.
        text: String,
    },
    /// Not three fields.
    #[error("expected `<old> <new> <ref>`: {line:?}")]
    Shape {
        /// The whole line.
        line: String,
    },
}

/// What a pushed ref points at once tags are peeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectType {
    /// A commit: the spec's path.
    Commit,
    /// A bare tree: scanned whole.
    Tree,
    /// A bare blob: one entry at path `/`.
    Blob,
}

/// A blob at a path, as a new commit adds or changes it, or as a tree holds it. The path is
/// bytes: git paths need not be UTF-8, and two distinct paths must never merge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Entry {
    path: Vec<u8>,
    sha: Sha,
}

impl Entry {
    /// The blob `sha` at `path`.
    pub fn new(path: Vec<u8>, sha: Sha) -> Entry {
        Entry { path, sha }
    }

    /// The path's raw bytes.
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// The path for rules and display: `String::from_utf8_lossy`.
    pub fn path_lossy(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }

    /// The blob's name.
    pub fn sha(&self) -> &Sha {
        &self.sha
    }
}

/// A path whose blob differs between two tips, with the blob on each side. A side is `None`
/// when the path is absent there or is a gitlink.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TipEntry {
    path: Vec<u8>,
    old: Option<Sha>,
    new: Option<Sha>,
}

impl TipEntry {
    /// The path's raw bytes.
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// The path for rules and display: `String::from_utf8_lossy`.
    pub fn path_lossy(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }

    /// The blob at the old tip; `None` when absent there or a gitlink.
    pub fn old(&self) -> Option<&Sha> {
        self.old.as_ref()
    }

    /// The blob at the new tip; `None` when absent there or a gitlink.
    // An accessor paired with `old`, not a constructor.
    #[allow(clippy::new_ret_no_self, clippy::wrong_self_convention)]
    pub fn new(&self) -> Option<&Sha> {
        self.new.as_ref()
    }
}

/// Runs git against one repository. The child inherits this process's environment, so git's
/// quarantine variables reach it; `git_dir`, when set, is passed as `GIT_DIR`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Git {
    binary: PathBuf,
    git_dir: Option<PathBuf>,
}

/// What a git child's stderr is kept up to; the rest is read and dropped.
const STDERR_KEEP: u64 = 4096;

impl Git {
    /// `POTLUCK_GIT` (default `git`); the repository is whatever `GIT_DIR` and the cwd say.
    pub fn from_env() -> Git {
        let binary = std::env::var_os("POTLUCK_GIT").unwrap_or_else(|| OsString::from("git"));
        Git::new(PathBuf::from(binary), None)
    }

    /// A runner for `binary`, with `git_dir` passed as `GIT_DIR` when set.
    pub fn new(binary: PathBuf, git_dir: Option<PathBuf>) -> Git {
        Git { binary, git_dir }
    }

    /// `cat-file --batch-check` on `<sha>^{}`: the peeled object and its type. A missing
    /// object or a type other than commit, tree or blob is `GitCmdError::Output`.
    pub fn peel(&self, sha: &Sha) -> Result<(Sha, ObjectType), GitCmdError> {
        const SUB: &str = "cat-file";
        let out = self.run(
            SUB,
            &[OsStr::new("--batch-check")],
            Some(format!("{sha}^{{}}\n").into_bytes()),
        )?;
        let text = String::from_utf8_lossy(&out);
        let line = text.lines().next().unwrap_or_default();
        let unexpected = || GitCmdError::Output {
            subcommand: SUB,
            detail: format!("{line:?}"),
        };
        // A found object answers `<sha> <type> <size>`; `<name> missing` has two fields.
        let [name, kind, _size] = line.split(' ').collect::<Vec<_>>()[..] else {
            return Err(unexpected());
        };
        let kind = match kind {
            "commit" => ObjectType::Commit,
            "tree" => ObjectType::Tree,
            "blob" => ObjectType::Blob,
            _ => return Err(unexpected()),
        };
        let peeled = Sha::parse(name).map_err(|_| unexpected())?;
        Ok((peeled, kind))
    }

    /// `rev-list <tip> --not --all <exclude..>`: commits new to the repository.
    pub fn new_commits(&self, tip: &Sha, exclude: &[Sha]) -> Result<Vec<Sha>, GitCmdError> {
        self.rev_list(&[], tip, exclude)
    }

    /// `rev-list --objects <tip> --not --all <exclude..>`: every new object's name.
    pub fn new_objects(&self, tip: &Sha, exclude: &[Sha]) -> Result<HashSet<Sha>, GitCmdError> {
        Ok(self
            .rev_list(&["--objects"], tip, exclude)?
            .into_iter()
            .collect())
    }

    /// `rev-list --objects --filter=object:type=tag --filter-provided-objects <tip> --not --all
    /// <exclude..>`: every annotated tag new to the repository from `tip` down to its target,
    /// `tip` itself included when it is one; empty when `tip` is a commit, tree or blob.
    pub fn new_tags(&self, tip: &Sha, exclude: &[Sha]) -> Result<Vec<Sha>, GitCmdError> {
        self.rev_list(
            &[
                "--objects",
                "--filter=object:type=tag",
                "--filter-provided-objects",
            ],
            tip,
            exclude,
        )
    }

    /// `diff-tree -r -m --root -z --stdin` over `commits`: each path a commit adds or changes
    /// against each of its parents (a root commit's whole tree), statuses `A`, `M`, `T`;
    /// deletions and mode-160000 entries left out; deduplicated by (path, sha) in first-seen
    /// order. Empty input yields empty output without running git. Any number of commits.
    pub fn changed_entries(&self, commits: &[Sha]) -> Result<Vec<Entry>, GitCmdError> {
        const SUB: &str = "diff-tree";
        if commits.is_empty() {
            return Ok(Vec::new());
        }
        let args = ["-r", "-m", "--root", "-z", "--stdin"].map(OsStr::new);
        let out = self.run(SUB, &args, Some(name_lines(commits)))?;
        let unexpected = |record: &[u8]| GitCmdError::Output {
            subcommand: SUB,
            detail: format!("{:?}", String::from_utf8_lossy(record)),
        };
        // Records alternate: a commit name on its own, or an entry header followed by its
        // path. Without rename detection every header carries exactly one path.
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
        while let Some(record) = records.next() {
            let Some(header) = record.strip_prefix(b":") else {
                parse_sha(record).ok_or_else(|| unexpected(record))?;
                continue;
            };
            let path = records.next().ok_or_else(|| unexpected(record))?;
            let fields: Vec<&[u8]> = header.split(|&b| b == b' ').collect();
            let [_old_mode, new_mode, _old_sha, new_sha, status] = fields[..] else {
                return Err(unexpected(record));
            };
            if !matches!(status, b"A" | b"M" | b"T") || new_mode == b"160000" {
                continue;
            }
            let entry = Entry::new(
                path.to_vec(),
                parse_sha(new_sha).ok_or_else(|| unexpected(record))?,
            );
            if seen.insert(entry.clone()) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// `diff-tree -r -z <old> <new>`: every path whose blob differs between the two trees,
    /// statuses `A`, `M`, `T` and `D`, with each side's blob or `None`.
    pub fn tip_diff(&self, old: &Sha, new: &Sha) -> Result<Vec<TipEntry>, GitCmdError> {
        const SUB: &str = "diff-tree";
        let args = ["-r", "-z", old.as_str(), new.as_str()].map(OsStr::new);
        let out = self.run(SUB, &args, None)?;
        let unexpected = |record: &[u8]| GitCmdError::Output {
            subcommand: SUB,
            detail: format!("{:?}", String::from_utf8_lossy(record)),
        };
        // The same header-then-path walk as `changed_entries`. A side that is all zeros (the
        // path is absent there) or a gitlink carries no blob, so it reads as `None`.
        let side = |mode: &[u8], sha: &[u8], record: &[u8]| -> Result<_, GitCmdError> {
            let sha = parse_sha(sha).ok_or_else(|| unexpected(record))?;
            let absent = mode == b"160000" || sha.as_str().bytes().all(|b| b == b'0');
            Ok((!absent).then_some(sha))
        };
        let mut entries = Vec::new();
        let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
        while let Some(record) = records.next() {
            let header = record
                .strip_prefix(b":")
                .ok_or_else(|| unexpected(record))?;
            let path = records.next().ok_or_else(|| unexpected(record))?;
            let fields: Vec<&[u8]> = header.split(|&b| b == b' ').collect();
            let [old_mode, new_mode, old_sha, new_sha, status] = fields[..] else {
                return Err(unexpected(record));
            };
            if !matches!(status, b"A" | b"M" | b"T" | b"D") {
                continue;
            }
            entries.push(TipEntry {
                path: path.to_vec(),
                old: side(old_mode, old_sha, record)?,
                new: side(new_mode, new_sha, record)?,
            });
        }
        Ok(entries)
    }

    /// `cat-file --batch-check` on `<rev>:<path>`: the blob there, `None` when the path is
    /// absent or names something other than a blob. A path with a newline cannot be asked.
    pub fn blob_at(&self, rev: &Sha, path: &str) -> Result<Option<Sha>, GitCmdError> {
        const SUB: &str = "cat-file";
        if path.contains('\n') {
            return Err(GitCmdError::Output {
                subcommand: SUB,
                detail: format!("a path with a newline: {path:?}"),
            });
        }
        let input = format!("{rev}:{path}\n").into_bytes();
        let out = self.run(SUB, &[OsStr::new("--batch-check")], Some(input))?;
        let line = out.split(|&b| b == b'\n').next().unwrap_or_default();
        match parse_header(line) {
            Some(Header::Found { sha, kind, .. }) => Ok((kind == b"blob").then_some(sha)),
            Some(Header::Missing { .. }) => Ok(None),
            None => Err(GitCmdError::Output {
                subcommand: SUB,
                detail: format!("{:?}", String::from_utf8_lossy(line)),
            }),
        }
    }

    /// `ls-tree -r -z <rev>`: every blob in the tree (mode 160000 left out); `rev` may be a
    /// commit, a tree or a tag of either.
    pub fn tree_entries(&self, rev: &Sha) -> Result<Vec<Entry>, GitCmdError> {
        const SUB: &str = "ls-tree";
        let args = [OsStr::new("-r"), OsStr::new("-z"), OsStr::new(rev.as_str())];
        let out = self.run(SUB, &args, None)?;
        let mut entries = Vec::new();
        for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let unexpected = || GitCmdError::Output {
                subcommand: SUB,
                detail: format!("{:?}", String::from_utf8_lossy(record)),
            };
            // `<mode> <type> <sha>\t<path>`: the path is everything after the first tab.
            let tab = record
                .iter()
                .position(|&b| b == b'\t')
                .ok_or_else(unexpected)?;
            let (meta, path) = (&record[..tab], &record[tab + 1..]);
            let fields: Vec<&[u8]> = meta.split(|&b| b == b' ').collect();
            let [_mode, kind, sha] = fields[..] else {
                return Err(unexpected());
            };
            if kind == b"blob" {
                entries.push(Entry::new(
                    path.to_vec(),
                    parse_sha(sha).ok_or_else(unexpected)?,
                ));
            }
        }
        Ok(entries)
    }

    /// `cat-file --batch-check` over `shas`: sizes of the names that are blobs; other types
    /// and missing names are left out. Any number of names.
    pub fn blob_sizes(&self, shas: &[Sha]) -> Result<HashMap<Sha, u64>, GitCmdError> {
        const SUB: &str = "cat-file";
        if shas.is_empty() {
            return Ok(HashMap::new());
        }
        let out = self.run(SUB, &[OsStr::new("--batch-check")], Some(name_lines(shas)))?;
        let mut sizes = HashMap::new();
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            match parse_header(line) {
                Some(Header::Found { sha, kind, size }) => {
                    if kind == b"blob" {
                        sizes.insert(sha, size);
                    }
                }
                Some(Header::Missing { .. }) => {}
                None => {
                    return Err(GitCmdError::Output {
                        subcommand: SUB,
                        detail: format!("{:?}", String::from_utf8_lossy(line)),
                    });
                }
            }
        }
        Ok(sizes)
    }

    /// `cat-file --batch` over `shas`, calling `f(sha, bytes)` once per blob in the order
    /// given as the stream arrives; non-blobs and missing names are skipped. One blob is in
    /// memory at a time, and callers pass only names at or under
    /// [`MAX_CONTENT_BYTES`](crate::kinds::generic::MAX_CONTENT_BYTES): content scanning is
    /// capped there to bound the hook's cost per push, so a larger blob gets path rules only and
    /// is never read. A reply for another name, a larger blob or an unreadable reply is
    /// `GitCmdError::Output`, with none of git's output in it.
    pub fn for_each_blob(
        &self,
        shas: &[Sha],
        f: impl FnMut(&Sha, &[u8]),
    ) -> Result<(), GitCmdError> {
        self.for_each_object(shas, Keep::Blobs, f)
    }

    /// `cat-file --batch` over `shas`, calling `f(sha, message)` once per commit or tag object
    /// in the order given: `message` is everything after the object's header block (the first
    /// blank line), empty when the object has none. Blobs, trees, missing names and objects
    /// whose size as `cat-file` reports it exceeds
    /// [`MAX_CONTENT_BYTES`](crate::kinds::generic::MAX_CONTENT_BYTES) are read past and
    /// skipped: content scanning is capped there to bound the hook's cost per push, so an
    /// over-cap message is deliberately left unscanned. A reply for another name or an
    /// unreadable reply is `GitCmdError::Output`, with none of git's output in it. Empty input
    /// yields no call and runs no git.
    pub fn for_each_message(
        &self,
        shas: &[Sha],
        mut f: impl FnMut(&Sha, &[u8]),
    ) -> Result<(), GitCmdError> {
        self.for_each_object(shas, Keep::Messages, |sha, body| {
            let message = body
                .windows(2)
                .position(|w| w == b"\n\n")
                .map_or(&[][..], |at| &body[at + 2..]);
            f(sha, message);
        })
    }

    /// The shared `cat-file --batch` stream: `f` sees each object `keep` admits, in order.
    fn for_each_object(
        &self,
        shas: &[Sha],
        keep: Keep,
        mut f: impl FnMut(&Sha, &[u8]),
    ) -> Result<(), GitCmdError> {
        const SUB: &str = "cat-file";
        if shas.is_empty() {
            return Ok(());
        }
        let mut running = self.spawn(SUB, &[OsStr::new("--batch")], Some(name_lines(shas)))?;
        let Some(stdout) = running.child.stdout.take() else {
            running.abort();
            return Err(GitCmdError::Output {
                subcommand: SUB,
                detail: "no stdout".to_owned(),
            });
        };
        let mut reader = BufReader::new(stdout);
        let mut header = Vec::new();
        let mut body = Vec::new();
        // One reply per name, in order: a header, then for a found object exactly `size`
        // bytes and a newline. A body `keep` does not admit is read into the sink, never kept.
        for sha in shas {
            header.clear();
            match read_reply(&mut reader, sha, keep, &mut header, &mut body) {
                Ok(Reply::Kept) => f(sha, &body),
                Ok(Reply::Skipped) => {}
                Ok(Reply::Eof) => {
                    drop(reader);
                    // git quitting early usually means it failed; prefer its own reason.
                    running.finish()?;
                    return Err(GitCmdError::Output {
                        subcommand: SUB,
                        detail: "output ended early".to_owned(),
                    });
                }
                Err(err) => {
                    // Close stdout first: a wrapper's grandchild blocked writing to it would
                    // otherwise keep stderr open past the kill and hang the drain.
                    drop(reader);
                    running.abort();
                    return Err(err);
                }
            }
        }
        drop(reader);
        running.finish()
    }

    /// `rev-list <flags> <tip> --not --all <exclude..>`, keeping the first token of each line:
    /// with `--objects` a line may carry a path after the name, in any bytes.
    fn rev_list(
        &self,
        flags: &[&str],
        tip: &Sha,
        exclude: &[Sha],
    ) -> Result<Vec<Sha>, GitCmdError> {
        const SUB: &str = "rev-list";
        let mut args: Vec<&OsStr> = flags.iter().map(OsStr::new).collect();
        args.push(OsStr::new(tip.as_str()));
        args.push(OsStr::new("--not"));
        args.push(OsStr::new("--all"));
        args.extend(exclude.iter().map(|s| OsStr::new(s.as_str())));
        let out = self.run(SUB, &args, None)?;
        out.split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let name = line.split(|&b| b == b' ').next().unwrap_or_default();
                std::str::from_utf8(name)
                    .ok()
                    .and_then(|hex| Sha::parse(hex).ok())
                    .ok_or_else(|| GitCmdError::Output {
                        subcommand: SUB,
                        detail: format!("{:?}", String::from_utf8_lossy(line)),
                    })
            })
            .collect()
    }

    /// Starts `git <args>` with stdin, stdout and stderr piped, feeding `input` from its own
    /// thread and draining stderr on another (capped at STDERR_KEEP): git blocks once a pipe
    /// holds 64 KiB, so writing everything first and reading after deadlocks on large inputs or
    /// outputs. The caller reads stdout, then calls `finish`.
    fn spawn(
        &self,
        subcommand: &'static str,
        args: &[&OsStr],
        input: Option<Vec<u8>>,
    ) -> Result<Running, GitCmdError> {
        let mut cmd = Command::new(&self.binary);
        if let Some(dir) = &self.git_dir {
            cmd.env("GIT_DIR", dir);
        }
        cmd.arg(subcommand)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|source| GitCmdError::Spawn { subcommand, source })?;
        let writer = input.and_then(|bytes| {
            let mut stdin = child.stdin.take()?;
            // EPIPE here means git quit early; its exit status says why.
            Some(std::thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            }))
        });
        let stderr = child.stderr.take();
        let drain = std::thread::spawn(move || {
            let mut kept = Vec::new();
            if let Some(mut err) = stderr {
                let _ = (&mut err).take(STDERR_KEEP).read_to_end(&mut kept);
                let _ = std::io::copy(&mut err, &mut std::io::sink());
            }
            kept
        });
        Ok(Running {
            subcommand,
            child,
            writer,
            drain,
        })
    }

    /// `spawn`, read stdout to EOF, `finish`; returns stdout.
    fn run(
        &self,
        subcommand: &'static str,
        args: &[&OsStr],
        input: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, GitCmdError> {
        let mut running = self.spawn(subcommand, args, input)?;
        let mut out = Vec::new();
        let read = match running.child.stdout.take() {
            Some(mut stdout) => stdout.read_to_end(&mut out).map(|_| ()),
            None => Ok(()),
        };
        if let Err(source) = read {
            running.abort();
            return Err(GitCmdError::Spawn { subcommand, source });
        }
        running.finish()?;
        Ok(out)
    }
}

/// One name per line, for a `--stdin` or `--batch` git.
fn name_lines(shas: &[Sha]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(shas.len() * 65);
    for sha in shas {
        bytes.extend_from_slice(sha.as_str().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn parse_sha(bytes: &[u8]) -> Option<Sha> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|hex| Sha::parse(hex).ok())
}

/// A `cat-file --batch-check` or `--batch` header line.
enum Header<'a> {
    Found { sha: Sha, kind: &'a [u8], size: u64 },
    Missing { name: &'a [u8] },
}

/// `<sha> <type> <size>` or `<name> missing`; anything else is `None`.
fn parse_header(line: &[u8]) -> Option<Header<'_>> {
    let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    match fields[..] {
        [name, b"missing"] => Some(Header::Missing { name }),
        [sha, kind, size] => Some(Header::Found {
            sha: parse_sha(sha)?,
            kind,
            size: std::str::from_utf8(size).ok()?.parse().ok()?,
        }),
        _ => None,
    }
}

/// Which `cat-file --batch` bodies a stream keeps.
#[derive(Clone, Copy)]
enum Keep {
    /// Blobs; one over the content cap is an error, since callers pre-filter by size.
    Blobs,
    /// Commits and tags at or under the content cap; larger ones are skipped unscanned.
    Messages,
}

/// What one `cat-file --batch` reply was.
enum Reply {
    /// An object `Keep` admits, now in the body buffer.
    Kept,
    /// A missing name or an object `Keep` does not admit, whose body was discarded.
    Skipped,
    /// stdout closed before the reply's header.
    Eof,
}

/// Longest `cat-file --batch` header read: a 64-hex name, a type and a size fit well inside.
const HEADER_MAX: u64 = 256;

/// Reads the `cat-file --batch` reply for `wanted`: the header into `header`, an admitted
/// object's body into `body`. Once framing is in doubt the bytes may be blob content, so errors
/// carry a fixed detail and never git's output.
fn read_reply(
    reader: &mut impl BufRead,
    wanted: &Sha,
    keep: Keep,
    header: &mut Vec<u8>,
    body: &mut Vec<u8>,
) -> Result<Reply, GitCmdError> {
    const SUB: &str = "cat-file";
    let io = |source| GitCmdError::Spawn {
        subcommand: SUB,
        source,
    };
    let unexpected = |detail: &str| GitCmdError::Output {
        subcommand: SUB,
        detail: detail.to_owned(),
    };
    if (&mut *reader)
        .take(HEADER_MAX)
        .read_until(b'\n', header)
        .map_err(io)?
        == 0
    {
        return Ok(Reply::Eof);
    }
    let Some(line) = header.strip_suffix(b"\n") else {
        return Err(unexpected("unreadable batch header"));
    };
    let (kind, size) = match parse_header(line) {
        Some(Header::Missing { name }) if name == wanted.as_str().as_bytes() => {
            return Ok(Reply::Skipped);
        }
        Some(Header::Found { sha, kind, size }) if sha == *wanted => (kind, size),
        Some(_) => return Err(unexpected("batch reply for another object")),
        None => return Err(unexpected("unreadable batch header")),
    };
    let under_cap = size <= crate::kinds::generic::MAX_CONTENT_BYTES;
    let kept = match keep {
        Keep::Blobs if kind == b"blob" && !under_cap => {
            return Err(unexpected("blob over the content cap"));
        }
        Keep::Blobs => kind == b"blob",
        Keep::Messages => under_cap && (kind == b"commit" || kind == b"tag"),
    };
    let mut limited = reader.take(size);
    let read = if kept {
        body.clear();
        limited.read_to_end(body).map_err(io)? as u64
    } else {
        std::io::copy(&mut limited, &mut std::io::sink()).map_err(io)?
    };
    let mut newline = [0u8; 1];
    if read != size || reader.read_exact(&mut newline).is_err() || newline != *b"\n" {
        return Err(unexpected("truncated object body"));
    }
    Ok(if kept { Reply::Kept } else { Reply::Skipped })
}

/// A git child with its two helper threads; `finish` joins them and checks the exit status.
struct Running {
    subcommand: &'static str,
    child: Child,
    writer: Option<JoinHandle<()>>,
    drain: JoinHandle<Vec<u8>>,
}

impl Running {
    /// Waits for git, joins the threads, and turns a non-zero exit into `Failed` with the
    /// first kept stderr line. On an earlier parse failure call `abort` instead.
    fn finish(mut self) -> Result<(), GitCmdError> {
        let subcommand = self.subcommand;
        let status = self.child.wait();
        let stderr = self.join();
        let status = status.map_err(|source| GitCmdError::Spawn { subcommand, source })?;
        if status.success() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&stderr);
        Err(GitCmdError::Failed {
            subcommand,
            stderr: text.lines().next().unwrap_or_default().trim().to_owned(),
        })
    }

    /// Kills git first, so a writer blocked on a full stdin pipe wakes up, then joins both threads.
    fn abort(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join();
    }

    /// Joins the writer and the stderr drain; a panicked thread keeps nothing.
    fn join(self) -> Vec<u8> {
        if let Some(writer) = self.writer {
            let _ = writer.join();
        }
        self.drain.join().unwrap_or_default()
    }
}

/// Why a plumbing call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitCmdError {
    /// git could not be started, or its output could not be read.
    #[error("cannot run git {subcommand}: {source}")]
    Spawn {
        /// The subcommand.
        subcommand: &'static str,
        /// The io error.
        #[source]
        source: std::io::Error,
    },
    /// git exited non-zero.
    #[error("git {subcommand} failed: {stderr}")]
    Failed {
        /// The subcommand.
        subcommand: &'static str,
        /// Its first stderr line, trimmed.
        stderr: String,
    },
    /// git exited 0 with output this module cannot read.
    #[error("unexpected git {subcommand} output: {detail}")]
    Output {
        /// The subcommand.
        subcommand: &'static str,
        /// What was unexpected.
        detail: String,
    },
}
