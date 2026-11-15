//! `potluck admin user ...`: accounts.

use serde_json::{Value, json};

use super::client::Verb;
use super::{AdminError, Context, output, pretty};

/// The `user` verbs.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum UserCmd {
    /// Every account.
    List,
    /// Create an account; prints a generated password unless --password-stdin.
    Create {
        /// The login name.
        username: String,
        /// Defaults to the username.
        #[arg(long)]
        display_name: Option<String>,
        /// The account's email address.
        #[arg(long)]
        email: Option<String>,
        /// Make the account a server admin.
        #[arg(long)]
        server_admin: bool,
        /// Read the password from stdin (one line, up to EOF) instead of generating one.
        #[arg(long)]
        password_stdin: bool,
    },
}

pub(crate) async fn run(cmd: UserCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        UserCmd::List => {
            cx.call(Verb::Get, "/api/v1/users", None, &[200], |v| {
                output::users(v.as_array().map(Vec::as_slice).unwrap_or_default())
            })
            .await
        }
        UserCmd::Create {
            username,
            display_name,
            email,
            server_admin,
            password_stdin,
        } => {
            let password = cx.new_password(password_stdin)?;
            // The body becomes text at once and is never formatted anywhere else.
            let body = json!({
                "display_name": display_name.as_deref().unwrap_or(&username),
                "username": username,
                "email": email,
                "password": password.text,
                "server_admin": server_admin,
            })
            .to_string();
            let user = cx
                .fetch(Verb::Post, "/api/v1/users", Some(&body), &[201])
                .await?
                .unwrap_or(Value::Null);
            print_user(
                cx,
                &user,
                password.generated.then_some(password.text.as_str()),
            )
        }
    }
}

/// The only printer of a password: the `{"user", "password"}` wrap in JSON mode, else the row
/// followed by `password: <pw>` when this command generated it.
fn print_user(
    cx: &mut Context<'_>,
    user: &Value,
    generated: Option<&str>,
) -> Result<(), AdminError> {
    if cx.json() {
        let text = pretty(&json!({"user": user, "password": generated}))?;
        return cx.write(&text);
    }
    let mut text = output::users(std::slice::from_ref(user));
    if let Some(pw) = generated {
        text.push_str(&format!("password: {pw}\n"));
    }
    cx.write(&text)
}
