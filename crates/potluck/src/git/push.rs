//! What a receive-pack request asks for and what it achieved: the command list parsed from
//! the spooled body, the repository's refs after git exits, and the commands the two agree on.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::git::engine::GitError;

/// Forty zeros: how a command spells "no object" for a created or deleted ref.
pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// The most commands one receive-pack request may carry; more is `CommandError::TooMany`.
pub const MAX_COMMANDS: usize = 10_000;

/// One `<old> <new> <ref>` command of a receive-pack request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RefCommand {
    old: String,
    new: String,
    ref_name: String,
}

impl RefCommand {
    /// A command moving `ref_name` from `old` to `new`.
    pub fn new(old: &str, new: &str, ref_name: &str) -> Self {
        Self {
            old: old.to_owned(),
            new: new.to_owned(),
            ref_name: ref_name.to_owned(),
        }
    }

    /// The sha the client believes the ref holds; `ZERO_SHA` for a creation.
    pub fn old(&self) -> &str {
        &self.old
    }

    /// The sha asked for; `ZERO_SHA` for a deletion.
    pub fn new_sha(&self) -> &str {
        &self.new
    }

    /// The full ref name.
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }
}

/// A ref a push moved: the SSE payload and the `consumed_refs` entry. Serialises as
/// `{"ref", "old", "new"}`; `new` is `ZERO_SHA` for a deletion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct RefMove {
    #[serde(rename = "ref")]
    ref_name: String,
    old: String,
    new: String,
}

impl RefMove {
    /// A move of `ref_name` from `old` to `new`.
    pub fn new(ref_name: &str, old: &str, new: &str) -> Self {
        Self {
            ref_name: ref_name.to_owned(),
            old: old.to_owned(),
            new: new.to_owned(),
        }
    }

    /// The full ref name.
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }

    /// The sha before the push; `ZERO_SHA` for a creation.
    pub fn old(&self) -> &str {
        &self.old
    }

    /// The sha after the push; `ZERO_SHA` for a deletion.
    pub fn new_sha(&self) -> &str {
        &self.new
    }
}

/// Why a command list could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// Pkt-line `line` (1-based) is not `<old> <new> <ref>`, or a length is not hex.
    #[error("receive-pack command line {line} is malformed")]
    Malformed {
        /// The 1-based pkt-line number.
        line: usize,
    },
    /// The body ended before the flush that closes the command list.
    #[error("receive-pack command list is truncated")]
    Truncated,
    /// More than `limit` commands before the flush.
    #[error("receive-pack command list exceeds {limit} commands")]
    TooMany {
        /// The cap, `MAX_COMMANDS`.
        limit: usize,
    },
    /// Reading the body failed.
    #[error("cannot read the receive-pack body: {0}")]
    Io(#[source] std::io::Error),
}

/// Reads pkt-lines from `body` up to the first flush (`0000`) and returns the commands.
/// `shallow <sha>` lines are skipped; the first command's `\0capabilities` suffix and a trailing
/// `\n` are dropped; a sha is 40 or 64 hex digits and the ref is non-empty. Nothing past the
/// flush is read. The `MAX_COMMANDS + 1`th command is `TooMany` and reading stops there.
pub fn parse_commands<R: Read>(mut body: R) -> Result<Vec<RefCommand>, CommandError> {
    let mut commands = Vec::new();
    for line in 1.. {
        let malformed = CommandError::Malformed { line };
        let mut header = [0u8; 4];
        read_exact(&mut body, &mut header)?;
        let len = std::str::from_utf8(&header)
            .ok()
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .ok_or(CommandError::Malformed { line })?;
        // A flush ends the list; the other special lengths (delim, response-end) have no place here.
        match len {
            0 => break,
            1..=4 => return Err(malformed),
            _ => {}
        }
        let mut payload = vec![0u8; len - 4];
        read_exact(&mut body, &mut payload)?;
        let text = std::str::from_utf8(&payload).map_err(|_| CommandError::Malformed { line })?;
        let text = text.strip_suffix('\n').unwrap_or(text);
        if text.starts_with("shallow ") {
            continue;
        }
        let text = text.split('\0').next().unwrap_or(text);
        let mut parts = text.splitn(3, ' ');
        let (Some(old), Some(new), Some(ref_name)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed);
        };
        if !is_sha(old) || !is_sha(new) || ref_name.is_empty() {
            return Err(malformed);
        }
        // Checked before reading further, so an endless body is cut off at the cap.
        if commands.len() == MAX_COMMANDS {
            return Err(CommandError::TooMany {
                limit: MAX_COMMANDS,
            });
        }
        commands.push(RefCommand::new(old, new, ref_name));
    }
    Ok(commands)
}

/// `read_exact`, with an early end of the body reported as `Truncated`.
fn read_exact<R: Read>(body: &mut R, buf: &mut [u8]) -> Result<(), CommandError> {
    body.read_exact(buf).map_err(|e| match e.kind() {
        ErrorKind::UnexpectedEof => CommandError::Truncated,
        _ => CommandError::Io(e),
    })
}

/// A SHA-1 or SHA-256 object name in hex.
fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `git for-each-ref --format=%(objectname) %(refname)` over `repo` with `git`, as a map from
/// ref name to sha; a non-zero exit is `GitError::Failed`.
pub async fn list_refs(git: &Path, repo: &Path) -> Result<BTreeMap<String, String>, GitError> {
    // A clean environment, as `Repos::init` uses, so no user git config shapes the listing.
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("--git-dir")
        .arg(repo)
        .arg("for-each-ref")
        .arg("--format=%(objectname) %(refname)")
        .stdin(Stdio::null());
    if let Some(search) = std::env::var_os("PATH") {
        command.env("PATH", search);
    }
    let output = command.output().await.map_err(GitError::Spawn)?;
    if !output.status.success() {
        return Err(GitError::Failed {
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(sha, name)| (name.to_owned(), sha.to_owned()))
        .collect())
}

/// The commands the repository confirms: a command is a move when the ref's sha differs between
/// `before` and `after` and `after` holds the command's `new` (`ZERO_SHA` meaning absent).
/// `old == new` is never a move. A delete of a ref absent in `before`, or a create of a ref already
/// at `new`, is a command git refused and is not a move. Order follows `commands`.
pub fn moved(
    commands: &[RefCommand],
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<RefMove> {
    // An unchanged ref covers both refusals named above: absent before and after, or at `new`
    // both times.
    commands
        .iter()
        .filter(|cmd| cmd.old != cmd.new)
        .filter(|cmd| {
            let was = before.get(&cmd.ref_name);
            let now = after.get(&cmd.ref_name);
            let wanted = (cmd.new != ZERO_SHA).then_some(&cmd.new);
            was != now && now == wanted
        })
        .map(|cmd| RefMove::new(&cmd.ref_name, &cmd.old, &cmd.new))
        .collect()
}
