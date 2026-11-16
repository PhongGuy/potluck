//! The check over one push, and the lines the client sees.
//!
//! Content over `MAX_CONTENT_BYTES`, blob or commit and tag message alike, is path-ruled but
//! never content-scanned: the cap bounds the hook's cost per push.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::kinds::{self, Finding, Kind, generic};
use crate::secret_check::git::{Entry, Git, GitCmdError, ObjectType, RefUpdate, Sha};

/// What the server told the hook through its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookEnv {
    kind: Option<String>,
    import: bool,
    override_id: Option<String>,
}

impl HookEnv {
    /// Reads `POTLUCK_KIND` (non-empty), `POTLUCK_IMPORT` (`1` means import) and
    /// `POTLUCK_OVERRIDE` (non-empty); an unset variable and an empty one mean the same.
    pub fn from_env() -> HookEnv {
        let text = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        HookEnv::new(
            text("POTLUCK_KIND").as_deref(),
            text("POTLUCK_IMPORT").as_deref() == Some("1"),
            text("POTLUCK_OVERRIDE").as_deref(),
        )
    }

    /// Builds the environment from its parts; an empty kind or override counts as none.
    pub fn new(kind: Option<&str>, import: bool, override_id: Option<&str>) -> HookEnv {
        let non_empty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        HookEnv {
            kind: non_empty(kind),
            import,
            override_id: non_empty(override_id),
        }
    }

    /// The workspace kind, if one was given.
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }

    /// Whether this push is an import.
    pub fn is_import(&self) -> bool {
        self.import
    }

    /// The admin override id, if one applies to this push.
    pub fn override_id(&self) -> Option<&str> {
        self.override_id.as_deref()
    }
}

/// One refused path under one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Rejection {
    ref_name: Vec<u8>,
    path: Vec<u8>,
    finding: Finding,
}

impl Rejection {
    /// A refusal of `path` under `ref_name` for `finding`.
    pub fn new(ref_name: &[u8], path: &[u8], finding: Finding) -> Rejection {
        Rejection {
            ref_name: ref_name.to_vec(),
            path: path.to_vec(),
            finding,
        }
    }

    /// The ref the path was pushed under.
    pub fn ref_name(&self) -> &[u8] {
        &self.ref_name
    }

    /// The refused path, as git stores it; `/` for a finding that has no tree path.
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    /// The rule that refused it.
    pub fn finding(&self) -> &Finding {
        &self.finding
    }
}

/// The check for one push. `Ok(vec![])` accepts. An override accepts without touching git.
/// Deletions are skipped. Per ref, in stdin order, the target is peeled: a commit yields its
/// new commits (`--not --all` plus every earlier ref's tip), path rules (generic, then the
/// kind) over every changed entry, and content rules over the entries whose blob is new to
/// the repository or whose path the kind's `content_path` accepts, each `(blob, path)` pair
/// once per push; in import mode the tip tree replaces both lists. A tree target is scanned
/// whole (every entry a candidate); a blob target is one entry at path `/`. Blobs over
/// `MAX_CONTENT_BYTES` are not read: the generic set skips them, and a kind refuses one at a
/// path it owns through `over_cap_rule`. Per ref the path-rule rejections come first, then
/// the content-rule ones, in entry order; a path refused by a path rule is not content-checked.
/// The generic content rules also run over the message of every new commit and every new
/// annotated tag behind the ref (the pushed tag's chain included), each object once per push
/// under the first ref that lists it, reported at path `/` after the ref's content hits; import
/// mode scans no message. A commit or tag object over `MAX_CONTENT_BYTES` is skipped like a
/// blob. An unknown or unset kind means generic rules only.
pub fn scan(env: &HookEnv, updates: &[RefUpdate], git: &Git) -> Result<Vec<Rejection>, ScanError> {
    if env.override_id().is_some() {
        return Ok(Vec::new());
    }
    let kind = env.kind().and_then(kinds::by_name);
    // Hits are keyed (ref index, pass, entry index) so the final sort gives each ref's
    // path-rule hits first, then its content hits, each in entry order.
    let mut hits: Vec<((usize, u8, usize), Rejection)> = Vec::new();
    let mut candidates: HashMap<Sha, Vec<(usize, usize, Vec<u8>)>> = HashMap::new();
    let mut seen: Vec<Sha> = Vec::new();
    // A (blob, path) pair is content-checked once, under the first ref that brings it.
    let mut queued: HashSet<(Sha, Vec<u8>)> = HashSet::new();
    // Message candidates in queue order; the map both finds a streamed object's (ref, index)
    // and keeps each object to the first ref that lists it.
    let mut messages: Vec<Sha> = Vec::new();
    let mut message_slots: HashMap<Sha, (usize, usize)> = HashMap::new();
    for (ri, update) in updates.iter().enumerate() {
        if update.is_delete() {
            continue;
        }
        let (target, object) = git.peel(update.new())?;
        let tags = if env.is_import() {
            Vec::new()
        } else {
            git.new_tags(update.new(), &seen)?
        };
        let (entries, fresh, commits) = match object {
            ObjectType::Commit if !env.is_import() => {
                // Commits shared with an earlier ref are listed once, under that ref; a blob
                // counts as new under every ref, since the kind's content rule depends on path.
                let commits = git.new_commits(&target, &seen)?;
                let fresh = git.new_objects(&target, &[])?;
                let entries = git.changed_entries(&commits)?;
                seen.push(target);
                (entries, Some(fresh), commits)
            }
            ObjectType::Commit | ObjectType::Tree => (git.tree_entries(&target)?, None, Vec::new()),
            ObjectType::Blob => (vec![Entry::new(b"/".to_vec(), target)], None, Vec::new()),
        };
        for sha in commits.into_iter().chain(tags) {
            if !message_slots.contains_key(&sha) {
                message_slots.insert(sha.clone(), (ri, messages.len()));
                messages.push(sha);
            }
        }
        for (ei, entry) in entries.into_iter().enumerate() {
            let path = entry.path_lossy();
            let refused =
                generic::path_rule(&path).or_else(|| kind.and_then(|k| k.path_rule(&path)));
            if let Some(finding) = refused {
                hits.push((
                    (ri, 0, ei),
                    Rejection::new(update.name(), entry.path(), finding),
                ));
            } else if (fresh.as_ref().is_none_or(|f| f.contains(entry.sha()))
                || kind.is_some_and(|k| k.content_path(&path)))
                && queued.insert((entry.sha().clone(), entry.path().to_vec()))
            {
                candidates.entry(entry.sha().clone()).or_default().push((
                    ri,
                    ei,
                    entry.path().to_vec(),
                ));
            }
        }
    }
    content_pass(git, kind, updates, candidates, &mut hits)?;
    git.for_each_message(&messages, |sha, message| {
        if let (Some(&(ri, mi)), Some(finding)) =
            (message_slots.get(sha), generic::content_rule(message))
        {
            hits.push((
                (ri, 2, mi),
                Rejection::new(updates[ri].name(), b"/", finding),
            ));
        }
    })?;
    hits.sort_by_key(|(key, _)| *key);
    Ok(hits.into_iter().map(|(_, r)| r).collect())
}

/// Streams every candidate blob at or under the cap once and runs the content rules for each
/// path it sits at: the generic set on the bytes, then the kind's rule with the path. A blob
/// over the cap is never read; only the kind's `over_cap_rule` may refuse it, by path alone.
fn content_pass(
    git: &Git,
    kind: Option<&'static dyn Kind>,
    updates: &[RefUpdate],
    candidates: HashMap<Sha, Vec<(usize, usize, Vec<u8>)>>,
    hits: &mut Vec<((usize, u8, usize), Rejection)>,
) -> Result<(), ScanError> {
    let names: Vec<Sha> = candidates.keys().cloned().collect();
    let sizes = git.blob_sizes(&names)?;
    // Every candidate is a blob git has just named, so a missing size fails closed.
    let mut kept = Vec::with_capacity(names.len());
    for sha in names {
        match sizes.get(&sha) {
            Some(&n) if n <= generic::MAX_CONTENT_BYTES => kept.push(sha),
            Some(_) => {
                for (ri, ei, path) in candidates.get(&sha).into_iter().flatten() {
                    let lossy = String::from_utf8_lossy(path);
                    if let Some(finding) = kind
                        .filter(|k| k.content_path(&lossy))
                        .and_then(|k| k.over_cap_rule(&lossy))
                    {
                        hits.push((
                            (*ri, 1, *ei),
                            Rejection::new(updates[*ri].name(), path, finding),
                        ));
                    }
                }
            }
            None => {
                return Err(ScanError::Git(GitCmdError::Output {
                    subcommand: "cat-file",
                    detail: "no blob size for a candidate".to_owned(),
                }));
            }
        }
    }
    git.for_each_blob(&kept, |sha, bytes| {
        let generic_hit = generic::content_rule(bytes);
        for (ri, ei, path) in candidates.get(sha).into_iter().flatten() {
            let lossy = String::from_utf8_lossy(path);
            let finding = generic_hit
                .clone()
                .or_else(|| kind.and_then(|k| k.content_rule(&lossy, bytes)));
            if let Some(finding) = finding {
                hits.push((
                    (*ri, 1, *ei),
                    Rejection::new(updates[*ri].name(), path, finding),
                ));
            }
        }
    })?;
    Ok(())
}

/// Why the check could not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanError {
    /// A plumbing call failed.
    #[error(transparent)]
    Git(#[from] GitCmdError),
}

/// How many `potluck-reject` lines are printed at most.
pub const MAX_LINES: usize = 20;

/// `potluck-reject <code> <ref> <path> <rule>\n` for the first `MAX_LINES` rejections, then
/// `potluck-reject-more <n>\n` when `n` remain. The path and the ref are percent-encoded:
/// `%`, space, bytes below 0x20, 0x7f and every byte of an invalid UTF-8 sequence become
/// `%XX` (upper-case hex); every other byte is written as is. The line splits on spaces into
/// exactly five fields. A finding without a tree path (a commit or tag message, or a tag whose
/// target is a blob) carries the path `/`, which the encoding leaves as is and which no git
/// tree path can start with.
pub fn report(rejections: &[Rejection], out: &mut impl Write) -> std::io::Result<()> {
    for r in rejections.iter().take(MAX_LINES) {
        writeln!(
            out,
            "potluck-reject {} {} {} {}",
            r.finding.code().as_str(),
            percent_encode(&r.ref_name),
            percent_encode(&r.path),
            r.finding.rule(),
        )?;
    }
    let rest = rejections.len().saturating_sub(MAX_LINES);
    if rest > 0 {
        writeln!(out, "potluck-reject-more {rest}")?;
    }
    Ok(())
}

/// The encoding `report` applies to one field.
pub fn percent_encode(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    // Valid UTF-8 stays readable except for the bytes that would break the line's fields;
    // each byte of an invalid sequence is escaped so the client can restore the exact path.
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '%' || c == ' ' || c < ' ' || c == '\x7f' {
                push_escape(&mut out, c as u8);
            } else {
                out.push(c);
            }
        }
        for &b in chunk.invalid() {
            push_escape(&mut out, b);
        }
    }
    out
}

fn push_escape(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
}
