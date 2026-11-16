//! The pre-receive hook that `core.hooksPath` points every receive-pack at.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The script: `#!/bin/sh`, a comment, and `POTLUCK_GIT='<git>' exec '<potluck_exe>' hook
/// pre-receive`, both paths single-quoted for sh (`'` becomes `'\''`). `InvalidData` when a
/// path is not UTF-8.
pub fn script(potluck_exe: &Path, git: &Path) -> io::Result<String> {
    Ok(format!(
        "#!/bin/sh\n# Written by potluck at every start; edits are lost. Runs the secret check.\nPOTLUCK_GIT={} exec {} hook pre-receive\n",
        sh_quote(git)?,
        sh_quote(potluck_exe)?,
    ))
}

/// `path` inside single quotes, each `'` closed, escaped and reopened, so sh reads it verbatim.
fn sh_quote(path: &Path) -> io::Result<String> {
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("hook path is not UTF-8: {}", path.display()),
        )
    })?;
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

/// Writes `script(potluck_exe, git)` to `<hooks_dir>/pre-receive` with mode 0755, replacing
/// whatever is there, through a temporary name and `rename` so a push in flight never execs a
/// half-written file. Returns the hook's path. On failure the temporary file is removed.
pub fn install(hooks_dir: &Path, potluck_exe: &Path, git: &Path) -> io::Result<PathBuf> {
    let text = script(potluck_exe, git)?;
    let hook = hooks_dir.join("pre-receive");
    let tmp = hook.with_extension("tmp");
    let placed = std::fs::write(&tmp, text)
        .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&tmp, &hook));
    // Leave no stray temp file behind; the step's own error is the one worth reporting.
    if let Err(err) = placed {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(hook)
}
