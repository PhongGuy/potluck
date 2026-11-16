//! The pre-receive secret check.

pub mod git;
pub mod scan;

use std::io::{Read, Write};
use std::process::ExitCode;

use crate::secret_check::git::{Git, GitCmdError, RefUpdate};
use crate::secret_check::scan::{HookEnv, ScanError, report, scan};

/// Reads stdin to EOF, parses every non-empty line, scans with `HookEnv::from_env()` and
/// `Git::from_env()`, writes the report to stderr. Exit 0 accepts; exit 1 refuses. A set but
/// unknown `POTLUCK_KIND` first writes `potluck-warning unknown kind "<name>"; generic rules
/// only`. A malformed line or a git failure writes one `potluck-error <reason>` line and exits
/// 1 (fail closed). Never returns `Err`, so main prints nothing extra.
pub fn run_pre_receive() -> ExitCode {
    let env = HookEnv::from_env();
    // A failed stderr write means git's end of the pipe is gone; the exit code still refuses.
    let mut err = std::io::stderr().lock();
    if let Some(name) = env.kind().filter(|k| crate::kinds::by_name(k).is_none()) {
        let _ = writeln!(
            err,
            "potluck-warning unknown kind {name:?}; generic rules only"
        );
    }
    match check(&env) {
        Ok(rejections) if rejections.is_empty() => ExitCode::SUCCESS,
        Ok(rejections) => {
            let _ = report(&rejections, &mut err);
            ExitCode::FAILURE
        }
        Err(reason) => {
            let _ = writeln!(err, "potluck-error {reason}");
            ExitCode::FAILURE
        }
    }
}

/// Parses stdin and scans it; the error is the reason for the `potluck-error` line, a fixed
/// ASCII phrase per failure that never carries git's output, a path or a ref name.
fn check(env: &HookEnv) -> Result<Vec<scan::Rejection>, String> {
    // Bytes, not text: a ref name need not be UTF-8, and every non-empty line is still parsed.
    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .map_err(|_| "unreadable stdin".to_owned())?;
    let updates = input
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(RefUpdate::parse_line)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "malformed ref line".to_owned())?;
    scan(env, &updates, &Git::from_env()).map_err(|ScanError::Git(e)| match e {
        GitCmdError::Spawn { subcommand, .. } => format!("cannot run git {subcommand}"),
        GitCmdError::Failed { subcommand, .. } => format!("git {subcommand} failed"),
        GitCmdError::Output { subcommand, .. } => format!("unexpected git {subcommand} output"),
    })
}
