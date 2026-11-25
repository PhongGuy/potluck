//! Error messages that carry a source leave it out of their own text, so a chain rendered the
//! way `main` prints it (anyhow's `{:#}`) names the cause once.

use std::io;
use std::path::PathBuf;

use potluck::git::GitError;
use potluck::git::RepoError;
use potluck::git::push::CommandError;
use potluck::secret_check::git::GitCmdError;
use potluck::server::ServerError;

const CAUSE: &str = "the distinctive io cause";

fn cause() -> io::Error {
    io::Error::other(CAUSE)
}

/// Renders `err` as `main` does and asserts the cause appears exactly once.
fn assert_cause_once(err: impl std::error::Error + Send + Sync + 'static) {
    let text = format!("{:#}", anyhow::Error::new(err));
    assert_eq!(text.matches(CAUSE).count(), 1, "{text}");
}

#[test]
fn hooks_error_names_its_cause_once() {
    assert_cause_once(ServerError::Hooks {
        path: PathBuf::from("/scratch/hooks"),
        source: cause(),
    });
}

#[test]
fn repo_io_error_names_its_cause_once() {
    assert_cause_once(RepoError::Io {
        path: PathBuf::from("/scratch/repo.git"),
        source: cause(),
    });
}

#[test]
fn git_cmd_spawn_error_names_its_cause_once() {
    assert_cause_once(GitCmdError::Spawn {
        subcommand: "cat-file",
        source: cause(),
    });
}

#[test]
fn command_list_io_error_names_its_cause_once() {
    assert_cause_once(CommandError::Io(cause()));
}

#[test]
fn git_body_and_spawn_errors_name_their_cause_once() {
    assert_cause_once(GitError::Body(cause()));
    assert_cause_once(GitError::Spawn(cause()));
}
