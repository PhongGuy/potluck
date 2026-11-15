//! `potluck admin user ...`: accounts.

use super::client::Verb;
use super::{AdminError, Context, output};

/// The `user` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum UserCmd {
    /// Every account.
    List,
}

pub(crate) async fn run(cmd: UserCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        UserCmd::List => {
            cx.call(Verb::Get, "/api/v1/users", None, &[200], |v| {
                output::users(v.as_array().map(Vec::as_slice).unwrap_or_default())
            })
            .await
        }
    }
}
