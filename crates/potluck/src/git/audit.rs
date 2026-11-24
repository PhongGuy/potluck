//! A startup check that the workspace rows and the repositories under `repos/` agree.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::slug::Slug;

/// One disagreement between the rows and `repos/`. Names read from disk are escaped and cut short,
/// so they are safe to log; no variant carries an absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mismatch {
    /// A row whose repository does not exist.
    Missing(String),
    /// A row whose repository path is a symlink or a file.
    NotDirectory(String),
    /// A `<team>/<ws>.git` directory with no row.
    Orphan(String),
    /// Any other entry under `repos/` or a team directory.
    Unexpected(String),
    /// A path that could not be read for a reason other than not existing.
    CouldNotCheck(String),
}

/// Compares `rows`, as `(team slug, workspace slug)` pairs, with the repositories under `root`.
/// Never follows a symlink and never changes anything on disk.
pub(crate) fn audit_repos(root: &Path, rows: &[(String, String)]) -> Vec<Mismatch> {
    // An unreadable root says nothing about any one workspace, so it is the only finding.
    let teams = match std::fs::read_dir(root) {
        Ok(read) => Some(read),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(_) => return vec![Mismatch::CouldNotCheck("repos".into())],
    };
    let mut found: Vec<Mismatch> = rows
        .iter()
        .filter_map(|(team, ws)| check_row(root, team, ws))
        .collect();
    let known: HashSet<(&str, &str)> = rows
        .iter()
        .map(|(team, ws)| (team.as_str(), ws.as_str()))
        .collect();
    for (name, entry) in sorted(teams, "repos", &mut found) {
        let team = name.to_string_lossy();
        let Ok(kind) = entry.file_type() else {
            found.push(Mismatch::CouldNotCheck(shown(&name)));
            continue;
        };
        if !kind.is_dir() || Slug::parse(&team).is_err() {
            found.push(Mismatch::Unexpected(shown(&name)));
            continue;
        }
        let repos = match std::fs::read_dir(entry.path()) {
            Ok(read) => Some(read),
            Err(_) => {
                found.push(Mismatch::CouldNotCheck(team.into_owned()));
                continue;
            }
        };
        for (name, entry) in sorted(repos, &team, &mut found) {
            let text = name.to_string_lossy();
            let ws = text
                .strip_suffix(".git")
                .filter(|ws| Slug::parse(ws).is_ok());
            let Ok(kind) = entry.file_type() else {
                found.push(Mismatch::CouldNotCheck(format!("{team}/{}", shown(&name))));
                continue;
            };
            match ws {
                Some(ws) if kind.is_dir() => {
                    if !known.contains(&(team.as_ref(), ws)) {
                        found.push(Mismatch::Orphan(format!("{team}/{ws}")));
                    }
                }
                _ => found.push(Mismatch::Unexpected(format!("{team}/{}", shown(&name)))),
            }
        }
    }
    found
}

/// Checks one row's team and repository without following either if it is a symlink.
fn check_row(root: &Path, team: &str, ws: &str) -> Option<Mismatch> {
    let label = format!("{team}/{ws}");
    for path in [root.join(team), root.join(team).join(format!("{ws}.git"))] {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Some(Mismatch::NotDirectory(label)),
            Err(err) if err.kind() == ErrorKind::NotFound => return Some(Mismatch::Missing(label)),
            Err(_) => return Some(Mismatch::CouldNotCheck(label)),
        }
    }
    None
}

/// The non-dot entries of a listing, by name. An entry that cannot be read is reported against
/// `dir` and skipped.
fn sorted(
    read: Option<std::fs::ReadDir>,
    dir: &str,
    found: &mut Vec<Mismatch>,
) -> Vec<(OsString, std::fs::DirEntry)> {
    let mut entries = Vec::new();
    for entry in read.into_iter().flatten() {
        match entry {
            Ok(entry) if entry.file_name().as_bytes().starts_with(b".") => {}
            Ok(entry) => entries.push((entry.file_name(), entry)),
            Err(_) => found.push(Mismatch::CouldNotCheck(dir.to_owned())),
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// A name read from disk, escaped so it cannot inject a log line and cut to a bounded length.
fn shown(name: &OsStr) -> String {
    name.to_string_lossy()
        .escape_debug()
        .take(MAX_SHOWN)
        .collect()
}

/// The most characters of an on-disk name that a mismatch carries.
const MAX_SHOWN: usize = 80;

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn row(team: &str, ws: &str) -> (String, String) {
        (team.to_owned(), ws.to_owned())
    }

    fn repo(root: &Path, team: &str, ws: &str) {
        std::fs::create_dir_all(root.join(team).join(format!("{ws}.git"))).unwrap();
    }

    #[test]
    fn matching_rows_and_repositories_report_nothing() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path(), "core", "api");
        assert_eq!(
            audit_repos(dir.path(), &[row("core", "api")]),
            Vec::<Mismatch>::new()
        );
    }

    #[test]
    fn a_row_without_a_repository_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path(), "core", "api");
        let found = audit_repos(dir.path(), &[row("core", "api"), row("core", "gone")]);
        assert_eq!(found, vec![Mismatch::Missing("core/gone".into())]);
    }

    #[test]
    fn a_repository_without_a_row_is_an_orphan() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path(), "core", "stray");
        assert_eq!(
            audit_repos(dir.path(), &[]),
            vec![Mismatch::Orphan("core/stray".into())]
        );
    }

    #[test]
    fn a_symlinked_team_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        repo(elsewhere.path(), "real", "api");
        symlink(elsewhere.path().join("real"), dir.path().join("core")).unwrap();
        let found = audit_repos(dir.path(), &[row("core", "api")]);
        assert_eq!(
            found,
            vec![
                Mismatch::NotDirectory("core/api".into()),
                Mismatch::Unexpected("core".into()),
            ]
        );
    }

    #[test]
    fn a_symlinked_workspace_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("core")).unwrap();
        symlink(elsewhere.path(), dir.path().join("core/api.git")).unwrap();
        let found = audit_repos(dir.path(), &[row("core", "api")]);
        assert_eq!(
            found,
            vec![
                Mismatch::NotDirectory("core/api".into()),
                Mismatch::Unexpected("core/api.git".into()),
            ]
        );
    }

    #[test]
    fn a_file_named_like_a_repository_is_unexpected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("core")).unwrap();
        std::fs::write(dir.path().join("core/x.git"), "").unwrap();
        assert_eq!(
            audit_repos(dir.path(), &[]),
            vec![Mismatch::Unexpected("core/x.git".into())]
        );
    }

    #[test]
    fn dotfiles_and_empty_teams_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        std::fs::create_dir_all(dir.path().join(".cache")).unwrap();
        std::fs::create_dir_all(dir.path().join("core/.tmp")).unwrap();
        std::fs::write(dir.path().join("core/.keep"), "").unwrap();
        assert_eq!(audit_repos(dir.path(), &[]), Vec::<Mismatch>::new());
    }

    #[test]
    fn a_name_that_is_not_a_slug_is_unexpected_and_escaped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Bad\nTeam")).unwrap();
        std::fs::create_dir_all(dir.path().join("core/notes")).unwrap();
        std::fs::create_dir_all(dir.path().join(format!("core/{}.git", "a".repeat(200)))).unwrap();
        let found = audit_repos(dir.path(), &[]);
        assert!(
            found.contains(&Mismatch::Unexpected("Bad\\nTeam".into())),
            "{found:?}"
        );
        assert!(
            found.contains(&Mismatch::Unexpected("core/notes".into())),
            "{found:?}"
        );
        assert_eq!(found.len(), 3, "{found:?}");
        let long = found
            .iter()
            .find_map(|m| match m {
                Mismatch::Unexpected(name) if name.starts_with("core/aaa") => Some(name),
                _ => None,
            })
            .unwrap();
        assert!(long.chars().count() <= 90, "cut short: {long}");
    }

    #[test]
    fn an_unreadable_team_could_not_be_checked_rather_than_missing() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path(), "core", "api");
        let team = dir.path().join("core");
        std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o000)).unwrap();
        let found = audit_repos(dir.path(), &[row("core", "api")]);
        std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root bypasses permission bits, so the read succeeds and there is nothing to assert.
        if is_root() {
            return;
        }
        assert!(
            found.contains(&Mismatch::CouldNotCheck("core".into())),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|m| matches!(m, Mismatch::Missing(_))),
            "{found:?}"
        );
    }

    #[test]
    fn an_unreadable_root_is_one_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repos");
        repo(&root, "core", "api");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let found = audit_repos(&root, &[row("core", "api"), row("core", "web")]);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root bypasses permission bits, so the read succeeds and there is nothing to assert.
        if is_root() {
            return;
        }
        assert_eq!(found, vec![Mismatch::CouldNotCheck("repos".into())]);
    }

    fn is_root() -> bool {
        std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
            status
                .lines()
                .any(|line| line.starts_with("Uid:") && line.split_whitespace().nth(2) == Some("0"))
        })
    }
}
