//! The pre-receive secret check.

pub mod git;
pub mod scan;

use std::io::{Read, Write};
use std::process::ExitCode;

use crate::secret_check::git::{Git, RefUpdate};
use crate::secret_check::scan::{HookEnv, report, scan};

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
            // One line whatever the reason says, so the client's line parser stays in step.
            let _ = writeln!(err, "potluck-error {}", reason.replace(['\n', '\r'], " "));
            ExitCode::FAILURE
        }
    }
}

/// Parses stdin and scans it; the error is the reason for the `potluck-error` line, built
/// from `ParseError` or `ScanError`, neither of which carries blob content.
fn check(env: &HookEnv) -> Result<Vec<scan::Rejection>, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    let updates = input
        .lines()
        .filter(|l| !l.is_empty())
        .map(RefUpdate::parse_line)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    scan(env, &updates, &Git::from_env()).map_err(|e| e.to_string())
}
