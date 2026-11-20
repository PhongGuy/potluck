//! The check over one push, and the lines the client sees.
//!
//! Content over `MAX_CONTENT_BYTES`, blob or commit and tag message alike, is path-ruled but
//! never content-scanned: the cap bounds the hook's cost per push.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, hash_map};
use std::io::Write;

use potluck_kind::{Code, Role};

use crate::kinds::{self, Blob, Change, Finding, RULES_ABI, RuleSet, generic};
use crate::role::Role as AccessRole;
use crate::secret_check::git::{Entry, Git, GitCmdError, ObjectType, RefUpdate, Sha, TipEntry};

/// What the server told the hook through its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookEnv {
    kind: Option<String>,
    import: bool,
    override_id: Option<String>,
    role: AccessRole,
}

impl HookEnv {
    /// Reads `POTLUCK_KIND` (non-empty), `POTLUCK_IMPORT` (`1` means import),
    /// `POTLUCK_OVERRIDE` (non-empty) and `POTLUCK_ROLE`; an unset variable and an empty one mean
    /// the same, and an unset or unknown role is `write`.
    pub fn from_env() -> HookEnv {
        let text = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
        let role = text("POTLUCK_ROLE")
            .as_deref()
            .and_then(AccessRole::parse)
            .unwrap_or(AccessRole::Write);
        HookEnv::new(
            text("POTLUCK_KIND").as_deref(),
            text("POTLUCK_IMPORT").as_deref() == Some("1"),
            text("POTLUCK_OVERRIDE").as_deref(),
        )
        .with_role(role)
    }

    /// Builds the environment from its parts; an empty kind or override counts as none, and the
    /// pusher's role is `write` until `with_role` says otherwise.
    pub fn new(kind: Option<&str>, import: bool, override_id: Option<&str>) -> HookEnv {
        let non_empty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_owned);
        HookEnv {
            kind: non_empty(kind),
            import,
            override_id: non_empty(override_id),
            role: AccessRole::Write,
        }
    }

    /// The same environment with the pusher's role set to `role`.
    #[must_use]
    pub fn with_role(self, role: AccessRole) -> HookEnv {
        HookEnv { role, ..self }
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

    /// The pusher's role on the workspace.
    pub fn role(&self) -> AccessRole {
        self.role
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

// A finding at the path the scan asked about keeps the entry's raw bytes, so a non-UTF-8
// path survives; a finding a rule set placed elsewhere carries its own path.
fn rejection(ref_name: &[u8], raw: &[u8], lossy: &str, finding: Finding) -> Rejection {
    let path = if finding.path() == lossy {
        raw.to_vec()
    } else {
        finding.path().as_bytes().to_vec()
    };
    Rejection {
        ref_name: ref_name.to_vec(),
        path,
        finding,
    }
}

/// The check for one push. `Ok(vec![])` accepts. An override accepts without touching git.
/// Deletions are skipped. Per ref, in stdin order, the target is peeled: a commit yields its
/// new commits (`--not --all` plus every earlier ref's tip), path rules (generic, then the
/// kind) over every changed entry, and content rules over the entries whose blob is new to
/// the repository or whose path the kind `reads`, each `(blob, path)` pair once per push; in
/// import mode the tip tree replaces both lists. A tree target is scanned whole (every entry a
/// candidate); a blob target is one entry at path `/`. Blobs over `MAX_CONTENT_BYTES` are not
/// read: the generic set skips them, and a kind may refuse one at a path it reads through
/// `check` with `Blob::TooLarge`. Per ref the path-rule rejections come first, then the
/// content-rule ones, in entry order; a path refused by a path rule is not content-checked.
/// The generic content rules also run over the message of every new commit and every new
/// annotated tag behind the ref (the pushed tag's chain included), each object once per push
/// under the first ref that lists it, reported at path `/` after the ref's content hits; import
/// mode scans no message. A commit or tag object over `MAX_CONTENT_BYTES` is skipped like a
/// blob. An unset kind means generic rules only; a kind the registry does not know refuses
/// with `ScanError::UnsupportedKind` unless every update is a delete. The kind's rule set runs
/// as `scan_with` describes.
pub fn scan(env: &HookEnv, updates: &[RefUpdate], git: &Git) -> Result<Vec<Rejection>, ScanError> {
    if env.override_id().is_some() {
        return Ok(Vec::new());
    }
    let kind = match env.kind() {
        None => None,
        Some(name) => match kinds::by_name(name) {
            Some(rules) => Some(rules),
            None if updates.iter().all(RefUpdate::is_delete) => None,
            None => return Err(ScanError::UnsupportedKind),
        },
    };
    scan_with(env, kind, updates, git)
}

/// `scan` with the rule set given rather than looked up, and no override check. Every call into
/// `rules` is panic-guarded; a panic, an `abi()` other than `RULES_ABI` (checked before any git
/// call) or a finding whose rule is empty or not `[a-z0-9_]` fails with `ScanError::Rules`.
/// After the history passes, every update whose old and new values both peel to commits gets a
/// tip pass: `check` with `Change::tip` at each path between the two tips that the kind `reads`.
/// Its findings sort after the ref's other hits, and a tip or format finding is dropped when an
/// earlier hit has the same ref, path and rule. When `env` names a kind and a rule set is given,
/// every created or format-touching update to a commit gets a format pass: its `format_file` must
/// hold a format the rule set lists, or the ref gets one `format_unsupported` finding, sorted
/// last. A `safety_relaxation` finding is dropped for an admin pusher.
pub fn scan_with(
    env: &HookEnv,
    rules: Option<&dyn RuleSet>,
    updates: &[RefUpdate],
    git: &Git,
) -> Result<Vec<Rejection>, ScanError> {
    if let Some(r) = rules
        && guarded(|| r.abi())? != RULES_ABI
    {
        return Err(ScanError::Rules);
    }
    let kind = GuardedRules(rules);
    let role = kind_role(env.role());
    // Passes 0 to 4 are path rule, content, message, tip and format, so the final sort groups
    // each ref's hits by pass in entry order, then in the order `check` returned them.
    let mut hits: Vec<(HitKey, Rejection)> = Vec::new();
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
            if let hash_map::Entry::Vacant(slot) = message_slots.entry(sha.clone()) {
                slot.insert((ri, messages.len()));
                messages.push(sha);
            }
        }
        for (ei, entry) in entries.into_iter().enumerate() {
            let path = entry.path_lossy();
            let refused = match generic::path_rule(&path) {
                Some(finding) => Some(finding),
                None => kind.path_rule(&path)?,
            };
            if let Some(finding) = refused {
                hits.push((
                    HitKey::new(ri, 0, ei, 0),
                    rejection(update.name(), entry.path(), &path, finding),
                ));
            } else if (fresh.as_ref().is_none_or(|f| f.contains(entry.sha()))
                || kind.reads(&path)?)
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
    content_pass(git, kind, role, updates, candidates, &mut hits)?;
    git.for_each_message(&messages, |sha, message| {
        if let (Some(&(ri, mi)), Some(finding)) =
            (message_slots.get(sha), generic::content_rule("/", message))
        {
            hits.push((
                HitKey::new(ri, 2, mi, 0),
                rejection(updates[ri].name(), b"/", "/", finding),
            ));
        }
    })?;
    // Each ref's tip diff is taken once and shared by the tip and format passes.
    let mut diffs = Vec::with_capacity(updates.len());
    for (ri, update) in updates.iter().enumerate() {
        let diff = ref_tip_diff(kind, update, git)?;
        for (at, r) in tip_pass(kind, update, diff.as_deref(), role, git)? {
            hits.push((HitKey::new(ri, 3, at.entry, at.finding), r));
        }
        diffs.push(diff);
    }
    // Only the server names a kind, so a rule set injected without one is never format-checked.
    if env.kind().is_some() {
        for (ri, update) in updates.iter().enumerate() {
            if let Some(r) = format_pass(kind, update, diffs[ri].as_deref(), git)? {
                hits.push((HitKey::new(ri, 4, 0, 0), r));
            }
        }
    }
    if role == Role::Admin {
        hits.retain(|(_, r)| r.finding.code() != Code::SafetyRelaxation);
    }
    // Passes 0 to 2 are already once per object per push; a tip or format hit is dropped only
    // when an earlier hit names the same ref, path and rule.
    hits.sort_by_key(|(key, _)| *key);
    let mut reported: HashSet<(Vec<u8>, Vec<u8>, String)> = HashSet::new();
    hits.retain(|(key, r)| {
        let fresh = reported.insert((
            r.ref_name.clone(),
            r.path.clone(),
            r.finding.rule().to_owned(),
        ));
        fresh || key.pass < 3
    });
    Ok(hits.into_iter().map(|(_, r)| r).collect())
}

/// The safety relaxations a push the server has already accepted would carry for a non-admin:
/// the tip pass alone over `updates`, as `Role::Write`, keeping only `safety_relaxation`
/// findings, each once per (ref, path, rule), in update order then tip-pass order. Fails like
/// `scan_with` on a git error or a failing rule set.
pub fn accepted_relaxations(
    rules: &dyn RuleSet,
    updates: &[RefUpdate],
    git: &Git,
) -> Result<Vec<Rejection>, ScanError> {
    if guarded(|| rules.abi())? != RULES_ABI {
        return Err(ScanError::Rules);
    }
    let mut found = Vec::new();
    // Two tip entries may yield a finding at the same path, so the key guards against a repeat.
    let mut logged: HashSet<(Vec<u8>, Vec<u8>, String)> = HashSet::new();
    for update in updates {
        let kind = GuardedRules(Some(rules));
        let diff = ref_tip_diff(kind, update, git)?;
        for (_, r) in tip_pass(kind, update, diff.as_deref(), Role::Write, git)? {
            if r.finding.code() == Code::SafetyRelaxation
                && logged.insert((
                    r.ref_name.clone(),
                    r.path.clone(),
                    r.finding.rule().to_owned(),
                ))
            {
                found.push(r);
            }
        }
    }
    Ok(found)
}

/// Where a hit sorts: by ref, then pass, then entry, then its place among one `check`'s findings.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HitKey {
    ref_index: usize,
    pass: u8,
    entry: usize,
    finding: usize,
}

impl HitKey {
    fn new(ref_index: usize, pass: u8, entry: usize, finding: usize) -> HitKey {
        HitKey {
            ref_index,
            pass,
            entry,
            finding,
        }
    }
}

/// A tip-pass hit's place within its ref: the tip entry, then the finding of that entry's `check`.
#[derive(Clone, Copy)]
struct TipHit {
    entry: usize,
    finding: usize,
}

/// The kind's view of the pusher's role.
fn kind_role(role: AccessRole) -> Role {
    match role {
        AccessRole::Read => Role::Read,
        AccessRole::Write => Role::Write,
        AccessRole::Admin => Role::Admin,
    }
}

/// One update's `tip_diff` of old tip against new tip, or `None` for a delete, a created ref,
/// an old or new value that is not a commit, and a push without a rule set; a peel failure is
/// a git error.
fn ref_tip_diff(
    kind: GuardedRules<'_>,
    update: &RefUpdate,
    git: &Git,
) -> Result<Option<Vec<TipEntry>>, ScanError> {
    if kind.0.is_none() || update.is_delete() || update.old().is_zero() {
        return Ok(None);
    }
    let (old, old_type) = git.peel(update.old())?;
    let (new, new_type) = git.peel(update.new())?;
    if old_type != ObjectType::Commit || new_type != ObjectType::Commit {
        return Ok(None);
    }
    Ok(Some(git.tip_diff(&old, &new)?))
}

/// Most blob bytes the tip pass holds at once; one batch's blobs are read and dropped before
/// the next batch is read. 32 caps keeps the extra spawns a few percent of the read time.
pub const TIP_BLOB_BUDGET: u64 = 32 * generic::MAX_CONTENT_BYTES;

const _: () = assert!(
    TIP_BLOB_BUDGET >= 2 * generic::MAX_CONTENT_BYTES,
    "an entry holds at most two sides, each at most the cap, so every entry fits one batch"
);

/// Judges one update old tip against new tip: `check` with `Change::tip` at every path the
/// kind `reads` among those `diff` (from `ref_tip_diff`) lists, as `(TipHit, rejection)` in
/// entry order, then `check` order; no diff yields nothing. Every finding is kept, whatever `role`.
fn tip_pass(
    kind: GuardedRules<'_>,
    update: &RefUpdate,
    diff: Option<&[TipEntry]>,
    role: Role,
    git: &Git,
) -> Result<Vec<(TipHit, Rejection)>, ScanError> {
    let Some(diff) = diff else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for (ei, entry) in diff.iter().enumerate() {
        let lossy = entry.path_lossy();
        if (entry.old().is_some() || entry.new().is_some()) && kind.reads(&lossy)? {
            entries.push((ei, entry, lossy));
        }
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let names: HashSet<Sha> = entries
        .iter()
        .flat_map(|(_, e, _)| e.old().into_iter().chain(e.new()))
        .cloned()
        .collect();
    let names: Vec<Sha> = names.into_iter().collect();
    let sizes = git.blob_sizes(&names)?;
    if names.iter().any(|sha| !sizes.contains_key(sha)) {
        return Err(no_size());
    }
    // Every name has a size from here on, so a lookup that misses cannot happen.
    let size = |sha: &Sha| sizes.get(sha).copied().unwrap_or_default();
    // Both sides of a change are needed at once, so entries are judged in batches whose blobs
    // at or under the cap are held together, up to TIP_BLOB_BUDGET bytes. A batch closes
    // before the entry that would take it past the budget; every entry fits an empty one.
    let mut found = Vec::new();
    let mut batch: Vec<&(usize, &TipEntry, String)> = Vec::new();
    let mut blobs: HashSet<Sha> = HashSet::new();
    let mut held = 0;
    for item in &entries {
        let (_, entry, _) = item;
        let mut own: Vec<&Sha> = entry.old().into_iter().chain(entry.new()).collect();
        own.retain(|sha| size(sha) <= generic::MAX_CONTENT_BYTES);
        own.dedup();
        let cost: u64 = own
            .iter()
            .filter(|sha| !blobs.contains(**sha))
            .map(|sha| size(sha))
            .sum();
        if !batch.is_empty() && held + cost > TIP_BLOB_BUDGET {
            judge_tip_batch(kind, update, role, git, &batch, &blobs, &mut found)?;
            batch.clear();
            blobs.clear();
            held = 0;
        }
        for sha in own {
            if blobs.insert(sha.clone()) {
                held += size(sha);
            }
        }
        batch.push(item);
    }
    judge_tip_batch(kind, update, role, git, &batch, &blobs, &mut found)?;
    Ok(found)
}

/// Reads one tip-pass batch's `blobs` in a single stream and runs `check` on each of its
/// entries in order, pushing findings onto `found`; the bytes are dropped on return.
fn judge_tip_batch(
    kind: GuardedRules<'_>,
    update: &RefUpdate,
    role: Role,
    git: &Git,
    batch: &[&(usize, &TipEntry, String)],
    blobs: &HashSet<Sha>,
    found: &mut Vec<(TipHit, Rejection)>,
) -> Result<(), ScanError> {
    let names: Vec<Sha> = blobs.iter().cloned().collect();
    let mut read: HashMap<Sha, Vec<u8>> = HashMap::with_capacity(names.len());
    git.for_each_blob(&names, |sha, bytes| {
        read.insert(sha.clone(), bytes.to_vec());
    })?;
    // Every side at or under the cap is in `blobs`, so a side not there is over the cap, and
    // one there that git did not stream fails closed, like a missing size.
    let side = |sha: Option<&Sha>| -> Result<Option<Blob<'_>>, ScanError> {
        let Some(sha) = sha else { return Ok(None) };
        match read.get(sha) {
            Some(bytes) => Ok(Some(Blob::Bytes(bytes))),
            None if !blobs.contains(sha) => Ok(Some(Blob::TooLarge)),
            None => Err(no_size()),
        }
    };
    for (ei, entry, lossy) in batch {
        let change = Change::tip(lossy, side(entry.old())?, side(entry.new())?, role);
        for (fi, finding) in kind.check(&change)?.into_iter().enumerate() {
            let at = TipHit {
                entry: *ei,
                finding: fi,
            };
            found.push((at, rejection(update.name(), entry.path(), lossy, finding)));
        }
    }
    Ok(())
}

/// Judges the workspace format at one update's new tip: `None` when the format file holds a
/// version the rule set lists, else a `format_unsupported` rejection at the format file. Only a
/// created ref (or one whose old value is not a commit) and an update whose tip diff lists the
/// format file are judged; a delete, a tree or blob target and a push without a rule set are not.
/// `diff` is the update's `ref_tip_diff`.
fn format_pass(
    kind: GuardedRules<'_>,
    update: &RefUpdate,
    diff: Option<&[TipEntry]>,
    git: &Git,
) -> Result<Option<Rejection>, ScanError> {
    let Some(rules) = kind.0 else { return Ok(None) };
    if update.is_delete() {
        return Ok(None);
    }
    let (new, new_type) = git.peel(update.new())?;
    if new_type != ObjectType::Commit {
        return Ok(None);
    }
    let file = guarded(|| rules.format_file())?;
    // An update that leaves the file alone keeps the format it was accepted with before.
    if diff.is_some_and(|d| !d.iter().any(|e| e.path() == file.as_bytes())) {
        return Ok(None);
    }
    // A missing file, a directory in its place and a file over the cap all read as no format.
    let mut version = None;
    if let Some(sha) = git.blob_at(&new, file)? {
        let shas = std::slice::from_ref(&sha);
        match git.blob_sizes(shas)?.get(&sha) {
            Some(&n) if n <= generic::MAX_CONTENT_BYTES => {
                let mut bytes = None;
                git.for_each_blob(shas, |_, b| bytes = Some(b.to_vec()))?;
                let bytes = bytes.ok_or_else(no_size)?;
                version = guarded(|| rules.detect_format(&bytes))?;
            }
            Some(_) => {}
            None => return Err(no_size()),
        }
    }
    let formats = guarded(|| rules.formats())?;
    if version.is_some_and(|v| formats.contains(&v.as_str())) {
        return Ok(None);
    }
    let finding = Finding::new(Code::FormatUnsupported, "format_unsupported", file);
    Ok(Some(Rejection::new(
        update.name(),
        file.as_bytes(),
        finding,
    )))
}

/// Every blob a pass reads is one git has just named, so a missing size or body fails closed.
fn no_size() -> ScanError {
    ScanError::Git(GitCmdError::Output {
        subcommand: "cat-file",
        detail: "no blob size for a candidate".to_owned(),
    })
}

/// Streams every candidate blob at or under the cap once and runs the content rules for each
/// path it sits at: the generic set on the bytes, then, when it finds nothing, every distinct
/// `check` finding of the kind. A blob over the cap is never read; only the kind's `check` with
/// `Blob::TooLarge` may refuse it.
fn content_pass(
    git: &Git,
    kind: GuardedRules<'_>,
    role: Role,
    updates: &[RefUpdate],
    candidates: HashMap<Sha, Vec<(usize, usize, Vec<u8>)>>,
    hits: &mut Vec<(HitKey, Rejection)>,
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
                    let findings = if kind.reads(&lossy)? {
                        kind.check(&Change::history(&lossy, Blob::TooLarge, role))?
                    } else {
                        Vec::new()
                    };
                    for (fi, finding) in findings.into_iter().enumerate() {
                        hits.push((
                            HitKey::new(*ri, 1, *ei, fi),
                            rejection(updates[*ri].name(), path, &lossy, finding),
                        ));
                    }
                }
            }
            None => return Err(no_size()),
        }
    }
    // The blob stream cannot stop early, so the first rule-set failure is kept and every later
    // blob is skipped.
    let mut failed = None;
    git.for_each_blob(&kept, |sha, bytes| {
        if failed.is_some() {
            return;
        }
        // The generic patterns depend on the bytes alone, so they run once per blob and the
        // hit is re-placed at each path the blob sits at.
        let generic_hit = generic::content_rule("", bytes);
        for (ri, ei, path) in candidates.get(sha).into_iter().flatten() {
            let lossy = String::from_utf8_lossy(path);
            let findings = match &generic_hit {
                Some(f) => vec![Finding::new(f.code(), f.rule(), &lossy)],
                None => match kind.check(&Change::history(&lossy, Blob::Bytes(bytes), role)) {
                    Ok(findings) => findings,
                    Err(e) => {
                        failed = Some(e);
                        return;
                    }
                },
            };
            for (fi, finding) in findings.into_iter().enumerate() {
                hits.push((
                    HitKey::new(*ri, 1, *ei, fi),
                    rejection(updates[*ri].name(), path, &lossy, finding),
                ));
            }
        }
    })?;
    failed.map_or(Ok(()), Err)
}

thread_local! {
    /// Set while this thread is inside a guarded rule-set call, for the quiet panic hook.
    static IN_RULE_CALL: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is inside a guarded rule-set call.
pub(crate) fn in_rule_call() -> bool {
    IN_RULE_CALL.try_with(Cell::get).unwrap_or(false)
}

/// Runs one call into a rule set, turning a panic into `ScanError::Rules`. The thread is
/// marked as in a rule call meanwhile; catch_unwind returns on unwind too, so the mark is
/// always restored.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, ScanError> {
    let outer = IN_RULE_CALL.replace(true);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    IN_RULE_CALL.set(outer);
    result.map_err(|_| ScanError::Rules)
}

/// The push's rule set, if any, reached only through guarded calls whose findings are checked.
#[derive(Clone, Copy)]
struct GuardedRules<'a>(Option<&'a dyn RuleSet>);

impl GuardedRules<'_> {
    fn path_rule(self, path: &str) -> Result<Option<Finding>, ScanError> {
        let Some(r) = self.0 else { return Ok(None) };
        guarded(|| r.path_rule(path))?.map(valid).transpose()
    }

    fn reads(self, path: &str) -> Result<bool, ScanError> {
        self.0.map_or(Ok(false), |r| guarded(|| r.reads(path)))
    }

    /// Every distinct finding of `check` in its order, the first of each (path, rule) kept, once
    /// every finding it returned has passed `valid`.
    fn check(self, change: &Change<'_>) -> Result<Vec<Finding>, ScanError> {
        let Some(r) = self.0 else {
            return Ok(Vec::new());
        };
        let findings = guarded(|| r.check(change))?
            .into_iter()
            .map(valid)
            .collect::<Result<Vec<_>, _>>()?;
        let mut kept: HashSet<(String, String)> = HashSet::new();
        Ok(findings
            .into_iter()
            .filter(|f| kept.insert((f.path().to_owned(), f.rule().to_owned())))
            .collect())
    }
}

/// A rule name goes on the wire as one field, so it must be non-empty `[a-z0-9_]`.
fn valid(finding: Finding) -> Result<Finding, ScanError> {
    let rule = finding.rule();
    let ok = !rule.is_empty()
        && rule
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(finding)
    } else {
        Err(ScanError::Rules)
    }
}

/// Why the check could not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanError {
    /// A plumbing call failed.
    #[error(transparent)]
    Git(#[from] GitCmdError),
    /// The kind's rule set panicked, has another ABI or returned a malformed finding.
    #[error("kind rules failed")]
    Rules,
    /// The workspace kind has no rule set on this server.
    #[error("unsupported kind")]
    UnsupportedKind,
}

/// How many `potluck-reject` lines are printed at most.
pub const MAX_LINES: usize = 20;

/// How many bytes of a finding's detail `report` prints at most.
pub const MAX_DETAIL_BYTES: usize = 200;

/// `potluck-reject <code> <ref> <path> <rule>\n` for the first `MAX_LINES` rejections, then
/// `potluck-reject-more <n>\n` when `n` remain. The path and the ref are percent-encoded:
/// `%`, space, bytes below 0x20, 0x7f and every byte of an invalid UTF-8 sequence become
/// `%XX` (upper-case hex); every other byte is written as is. The line splits on spaces into
/// exactly five fields. A finding without a tree path (a commit or tag message, or a tag whose
/// target is a blob) carries the path `/`, which the encoding leaves as is and which no git
/// tree path can start with. A finding with a non-empty detail is followed by
/// `potluck-reject-detail <detail>\n`, the detail cut to at most `MAX_DETAIL_BYTES` on a
/// character boundary and then encoded the same way; detail lines do not count toward the cap.
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
        if let Some(detail) = r.finding.detail().filter(|d| !d.is_empty()) {
            // Cut before encoding, so the cap bounds the rule set's text, not its escapes.
            let cut = &detail[..detail.floor_char_boundary(MAX_DETAIL_BYTES)];
            writeln!(
                out,
                "potluck-reject-detail {}",
                percent_encode(cut.as_bytes())
            )?;
        }
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
