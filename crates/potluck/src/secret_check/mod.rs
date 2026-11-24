//! The pre-receive secret check.

pub mod git;
pub mod scan;

use std::io::{Read, Write};
use std::process::ExitCode;

use crate::kinds::RuleSet;
use crate::secret_check::git::{Git, GitCmdError, RefUpdate};
use crate::secret_check::scan::{
    HookEnv, ScanError, accepted_relaxations, default_branch_rewrites, report, scan,
};

/// Reads stdin to EOF, parses every non-empty line, scans with `HookEnv::from_env()` and
/// `Git::from_env()`, writes the report to stderr. Exit 0 accepts; exit 1 refuses. A ref name
/// that is not valid UTF-8 writes `ref name is not valid UTF-8` and exits 1 before any scan or
/// git call. A malformed line, a git failure, a failing rule set or an unsupported kind writes
/// one `potluck-error <reason>` line and exits 1 (fail closed). Panics print nothing. Never
/// returns `Err`, so main prints nothing extra.
pub fn run_pre_receive() -> ExitCode {
    quiet_panics();
    let env = HookEnv::from_env();
    // A failed stderr write means git's end of the pipe is gone; the exit code still refuses.
    let mut err = std::io::stderr().lock();
    match check(&env) {
        Ok(rejections) if rejections.is_empty() => ExitCode::SUCCESS,
        Ok(rejections) => {
            let _ = report(&rejections, &mut err);
            write_report_file(&report_text(&rejections));
            ExitCode::FAILURE
        }
        Err(Refusal::RefNameNotUtf8) => {
            let _ = writeln!(err, "ref name is not valid UTF-8");
            write_report_file(HOOK_ERROR_LINE);
            ExitCode::FAILURE
        }
        Err(Refusal::Error(reason)) => {
            let _ = writeln!(err, "potluck-error {reason}");
            write_report_file(HOOK_ERROR_LINE);
            ExitCode::FAILURE
        }
    }
}

/// The variable naming the file the hook writes its refusal lines to.
pub const REPORT_ENV: &str = "POTLUCK_HOOK_REPORT";
/// At most this many lines are written.
pub const MAX_REPORT_LINES: usize = 64;
/// The report-file text for a refusal that carries no rule (bad input, failed scan).
const HOOK_ERROR_LINE: &str = "hook_error none\n";

/// One `<code> <rule>\n` line per distinct (code, rule), in first-seen order, at most 64.
pub fn report_text(rejections: &[scan::Rejection]) -> String {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for r in rejections {
        let key = (r.finding().code().as_str(), r.finding().rule());
        if !seen.contains(&key) {
            seen.push(key);
            if seen.len() == MAX_REPORT_LINES {
                break;
            }
        }
    }
    seen.iter()
        .map(|(code, rule)| format!("{code} {rule}\n"))
        .collect()
}

/// Overwrites the file `REPORT_ENV` names with `text`. The engine creates that file; the hook
/// never does, so a stale or wrong path grows nothing. Every failure is ignored: the exit code
/// alone decides the push.
fn write_report_file(text: &str) {
    let Some(path) = std::env::var_os(REPORT_ENV).filter(|p| !p.is_empty()) else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Logs, for a push the server accepted from an admin, one info line "safety relaxation
/// accepted" per relaxation `accepted_relaxations` finds, with `workspace_id`, `pusher`, the ref
/// (lossy UTF-8) and the rule. A failed re-check logs one warn line instead and never panics.
pub fn log_accepted_relaxations(
    workspace_id: i64,
    pusher: &str,
    rules: &dyn RuleSet,
    updates: &[RefUpdate],
    git: &Git,
) {
    match accepted_relaxations(rules, updates, git) {
        Ok(found) => {
            for r in found {
                let ref_name = String::from_utf8_lossy(r.ref_name());
                tracing::info!(
                    workspace_id,
                    pusher,
                    "ref" = %ref_name,
                    rule = r.finding().rule(),
                    "safety relaxation accepted"
                );
            }
        }
        // The error text may carry git's output, so only the push is named.
        Err(_) => tracing::warn!(
            workspace_id,
            pusher,
            "could not re-check an accepted admin push"
        ),
    }
}

/// Logs one info line `default branch rewritten` (`workspace_id`, `pusher`, `override_id`,
/// `ref`, `rule`) per force-push or delete of the protected branch in an accepted push; `None`
/// protects every branch. A failed check logs one warn line naming only the push. Never panics.
pub fn log_default_branch_rewrites(
    workspace_id: i64,
    pusher: &str,
    override_id: Option<i64>,
    default_branch: Option<&str>,
    updates: &[RefUpdate],
    git: &Git,
) {
    match default_branch_rewrites(default_branch, updates, git) {
        Ok(found) => {
            for r in found {
                let ref_name = String::from_utf8_lossy(r.ref_name());
                tracing::info!(
                    workspace_id,
                    pusher,
                    override_id,
                    "ref" = %ref_name,
                    rule = r.finding().rule(),
                    "default branch rewritten"
                );
            }
        }
        // The error text may carry git's output, so only the push is named.
        Err(_) => tracing::warn!(
            workspace_id,
            pusher,
            "could not re-check an accepted admin push"
        ),
    }
}

/// Installs a panic hook that prints nothing, so a rule set's panic message never reaches the
/// pushing client; the scan reports the panic as `kind rules failed`.
pub fn quiet_panics() {
    std::panic::set_hook(Box::new(|_| {}));
}

/// Installs, once, a panic hook that prints nothing for a panic inside a rule-set call and
/// hands every other panic to the hook that was there before. The scan already turns such a
/// panic into `ScanError::Rules`, and the caller logs that.
pub fn quiet_rule_panics() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !scan::in_rule_call() {
                previous(info);
            }
        }));
    });
}

/// Why `check` refused the push without a scan report.
enum Refusal {
    /// A pushed ref name is not valid UTF-8; the whole push is refused on the name alone.
    RefNameNotUtf8,
    /// The reason for the `potluck-error` line, a fixed ASCII phrase per failure that never
    /// carries git's output, a path or a ref name.
    Error(String),
}

/// Parses stdin, refuses any non-UTF-8 ref name, then scans.
fn check(env: &HookEnv) -> Result<Vec<scan::Rejection>, Refusal> {
    // Bytes, not text: a ref name need not be UTF-8, and every non-empty line is still parsed.
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .map_err(|_| Refusal::Error("unreadable stdin".to_owned()))?;
    let updates = input
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(RefUpdate::parse_line)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Refusal::Error("malformed ref line".to_owned()))?;
    // The name decides on its own, before any git call and even under an armed override, so a
    // ref the rest of the server cannot carry as text never lands.
    if updates
        .iter()
        .any(|u| std::str::from_utf8(u.name()).is_err())
    {
        return Err(Refusal::RefNameNotUtf8);
    }
    scan(env, &updates, &Git::from_env()).map_err(|e| {
        Refusal::Error(match e {
            ScanError::Git(GitCmdError::Spawn { subcommand, .. }) => {
                format!("cannot run git {subcommand}")
            }
            ScanError::Git(GitCmdError::Failed { subcommand, .. }) => {
                format!("git {subcommand} failed")
            }
            ScanError::Git(GitCmdError::Output { subcommand, .. }) => {
                format!("unexpected git {subcommand} output")
            }
            ScanError::Rules => "kind rules failed".to_owned(),
            ScanError::UnsupportedKind => "unsupported kind".to_owned(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kinds::{Code, Finding};
    use crate::secret_check::scan::Rejection;

    fn rej(code: Code, rule: &'static str, path: &str) -> Rejection {
        Rejection::new(
            b"refs/heads/main",
            path.as_bytes(),
            Finding::new(code, rule, path),
        )
    }

    #[test]
    fn the_report_has_one_line_per_distinct_code_and_rule() {
        let text = report_text(&[
            rej(Code::EnvFile, "env_file", ".env"),
            rej(Code::SecretPattern, "aws", "a.txt"),
            rej(Code::EnvFile, "env_file", "sub/.env"),
        ]);
        assert_eq!(text, "secret_env_file env_file\nsecret_pattern aws\n");
    }

    #[test]
    fn the_report_stops_at_64_lines() {
        let many: Vec<Rejection> = (0..100)
            .map(|i| {
                let rule: &'static str = Box::leak(format!("rule_{i}").into_boxed_str());
                rej(Code::SecretPattern, rule, "f")
            })
            .collect();
        let text = report_text(&many);
        assert_eq!(text.lines().count(), MAX_REPORT_LINES);
        assert!(text.starts_with("secret_pattern rule_0\n"), "{text}");
    }
}
