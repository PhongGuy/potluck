//! `potluck admin user ...`: accounts.

use serde_json::{Map, Value, json};

use super::client::Verb;
use super::{AdminError, Context, NewPassword, output, pretty, segment};

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
        /// Read the password from stdin to EOF, minus one trailing newline; else generate one.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Change fields; an absent flag leaves its field alone.
    Set {
        /// The account to change.
        username: String,
        /// A new display name.
        #[arg(long)]
        display_name: Option<String>,
        /// A new email; conflicts with --clear-email.
        #[arg(long, conflicts_with = "clear_email")]
        email: Option<String>,
        /// Remove the email.
        #[arg(long)]
        clear_email: bool,
        /// true or false.
        #[arg(long, value_name = "BOOL")]
        server_admin: Option<bool>,
        /// true or false; a disabled account cannot sign in and its tokens stop working.
        #[arg(long, value_name = "BOOL")]
        disabled: Option<bool>,
    },
    /// Set a new password: generated and printed once, or read from stdin.
    ResetPassword {
        /// The account to change.
        username: String,
        /// Read the password from stdin to EOF, minus one trailing newline; else generate one.
        #[arg(long)]
        password_stdin: bool,
    },
    /// The account's tokens.
    Tokens {
        /// The account whose tokens to list.
        username: String,
    },
}

/// Personal access tokens.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum TokenCmd {
    /// Revoke a token by its id, whoever owns it.
    Revoke {
        /// The token's id, as `user tokens` lists it.
        id: i64,
    },
}

pub(crate) async fn run(cmd: UserCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        UserCmd::List => {
            cx.call_list(Verb::Get, "/api/v1/users", None, &[200], output::users)
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
            let answer = cx
                .fetch(Verb::Post, "/api/v1/users", Some(&body), &[201])
                .await;
            print_answer(cx, answer, &password)
        }
        UserCmd::Set {
            username,
            display_name,
            email,
            clear_email,
            server_admin,
            disabled,
        } => {
            let username = segment("username", &username)?;
            // Only the given flags become keys, so the PATCH leaves every other field alone.
            let mut body = Map::new();
            if let Some(name) = display_name {
                body.insert("display_name".into(), name.into());
            }
            if let Some(email) = email {
                body.insert("email".into(), email.into());
            } else if clear_email {
                body.insert("email".into(), Value::Null);
            }
            if let Some(admin) = server_admin {
                body.insert("server_admin".into(), admin.into());
            }
            if let Some(disabled) = disabled {
                body.insert("disabled".into(), disabled.into());
            }
            let body = Value::Object(body).to_string();
            cx.call(
                Verb::Patch,
                &format!("/api/v1/users/{username}"),
                Some(&body),
                &[200],
                |user| output::users(std::slice::from_ref(user)),
            )
            .await
        }
        UserCmd::ResetPassword {
            username,
            password_stdin,
        } => {
            let username = segment("username", &username)?;
            let password = cx.new_password(password_stdin)?;
            let body = json!({"password": password.text}).to_string();
            let answer = cx
                .fetch(
                    Verb::Patch,
                    &format!("/api/v1/users/{username}"),
                    Some(&body),
                    &[200],
                )
                .await;
            print_answer(cx, answer, &password)
        }
        UserCmd::Tokens { username } => {
            let username = segment("username", &username)?;
            cx.call_list(
                Verb::Get,
                &format!("/api/v1/users/{username}/tokens"),
                None,
                &[200],
                output::tokens,
            )
            .await
        }
    }
}

pub(crate) async fn run_token(cmd: TokenCmd, cx: &mut Context<'_>) -> Result<(), AdminError> {
    match cmd {
        TokenCmd::Revoke { id } => {
            let target = format!("/api/v1/tokens/{id}");
            cx.confirm(Verb::Delete, &target, &format!("revoked token {id}\n"))
                .await
        }
    }
}

/// Prints the answer to a request that set `password`. An unreadable answer may still mean the
/// server applied the password, so a generated one is printed with no user before the error.
fn print_answer(
    cx: &mut Context<'_>,
    answer: Result<Option<Value>, AdminError>,
    password: &NewPassword,
) -> Result<(), AdminError> {
    let generated = password.generated.then_some(password.text.as_str());
    match answer {
        Ok(user) => print_user(cx, &user.unwrap_or(Value::Null), generated),
        Err(e @ AdminError::Malformed { .. }) if generated.is_some() => {
            print_user(cx, &Value::Null, generated)?;
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// The only printer of a password: the `{"user", "password"}` wrap in JSON mode, else the row
/// (none for a null user) followed by `password: <pw>` when this command generated it.
fn print_user(
    cx: &mut Context<'_>,
    user: &Value,
    generated: Option<&str>,
) -> Result<(), AdminError> {
    if cx.json() {
        let text = pretty(&json!({"user": user, "password": generated}))?;
        return cx.write(&text);
    }
    let mut text = if user.is_null() {
        String::new()
    } else {
        output::users(std::slice::from_ref(user))
    };
    if let Some(pw) = generated {
        text.push_str(&format!("password: {pw}\n"));
    }
    cx.write(&text)
}
