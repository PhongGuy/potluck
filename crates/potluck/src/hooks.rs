//! The pre-receive hook that `core.hooksPath` points every receive-pack at.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The pre-receive hook written at every start. Until the secret check lands it accepts
/// everything; the comment tells an operator why their edits vanish.
pub(crate) const PRE_RECEIVE: &str = "#!/bin/sh\n# Written by potluck at every start; edits are lost. The secret check arrives with a later release.\nexit 0\n";

/// Writes `<hooks_dir>/pre-receive` with mode 0755, replacing whatever is there, through a
/// temporary name and `rename` so a push in flight never execs a half-written file.
/// Returns the hook's path.
pub fn install(hooks_dir: &Path) -> io::Result<PathBuf> {
    let hook = hooks_dir.join("pre-receive");
    let tmp = hook.with_extension("tmp");
    std::fs::write(&tmp, PRE_RECEIVE)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &hook)?;
    Ok(hook)
}
