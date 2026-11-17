//! The potluck admin command line over the admin socket.

pub mod access;
pub mod client;
pub mod output;
pub mod teams;
pub mod update;
pub mod users;

use std::io::{self, Read, Write};
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use self::client::{AdminClient, ApiResponse, Verb};
use crate::slug::{Slug, SlugError};

/// `potluck admin`: one API call per verb, as the `system` actor.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct AdminArgs {
    /// The running server's data directory, where `admin.sock` lives.
    #[arg(long, global = true, env = "POTLUCK_DATA", default_value = "./data")]
    data: PathBuf,
    /// Print the answer as JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    noun: Noun,
}

/// What to manage.
#[derive(clap::Subcommand, Debug)]
#[non_exhaustive]
pub enum Noun {
    /// Accounts.
    #[command(subcommand)]
    User(users::UserCmd),
    /// Personal access tokens.
    #[command(subcommand)]
    Token(users::TokenCmd),
    /// Teams and their members.
    #[command(subcommand)]
    Team(teams::TeamCmd),
    /// Workspaces.
    #[command(subcommand)]
    Workspace(teams::WorkspaceCmd),
    /// Who may use a workspace, and with which role.
    #[command(subcommand)]
    Grant(access::GrantCmd),
    /// One-push exemptions from the secret check.
    #[command(subcommand)]
    Override(access::OverrideCmd),
    /// Whether a newer signed release exists.
    #[command(subcommand)]
    Update(update::UpdateCmd),
}

/// Why a command failed; `main` prints it after `error: `.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdminError {
    /// Nothing answers on the admin socket.
    #[error(
        "cannot reach the admin socket {path}: is potluck serve running on this data directory?"
    )]
    Unreachable {
        /// The socket that was tried.
        path: PathBuf,
        /// Why connecting failed.
        #[source]
        source: io::Error,
    },
    /// The connection broke or the answer could not be read.
    #[error("the request to the server failed")]
    Transport {
        /// What went wrong on the wire.
        #[source]
        source: io::Error,
    },
    /// The server refused; the error body as the API answered it.
    #[error("{code}: {message}{}", field_note(.field.as_deref()))]
    Api {
        /// The HTTP status.
        status: u16,
        /// The stable error code.
        code: String,
        /// The human-readable message.
        message: String,
        /// The request field the error names, if any.
        field: Option<String>,
    },
    /// A status the verb did not expect without the error model's body, or an expected one
    /// whose body is not the JSON the verb reads.
    #[error("the server answered HTTP {status} with a body this command cannot read")]
    Malformed {
        /// The HTTP status.
        status: u16,
    },
    /// Writing to stdout failed.
    #[error("cannot write the output")]
    Output(#[source] io::Error),
    /// `--password-stdin` could not read stdin, or it was not UTF-8.
    #[error("cannot read the password from stdin")]
    Stdin(#[source] io::Error),
    /// An argument that goes into the request path is not a valid name.
    #[error("invalid {arg} {value:?}: {reason}")]
    BadName {
        /// Which argument.
        arg: &'static str,
        /// The value as given.
        value: String,
        /// What the name rules refuse.
        #[source]
        reason: SlugError,
    },
    /// The OS random number generator failed while generating a password.
    #[error("cannot generate a password: the OS random number generator failed")]
    Rng,
}

fn field_note(field: Option<&str>) -> String {
    field.map(|f| format!(" (field {f})")).unwrap_or_default()
}

/// Runs one command: reads `stdin` only when a verb asks for it, writes results to `stdout`,
/// returns the failure for `main` to print. Holds the writer across awaits, so the future is
/// not `Send`: `cli::run` calls it directly, never through `tokio::spawn`.
pub async fn run(
    args: AdminArgs,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
) -> Result<(), AdminError> {
    let mut cx = Context {
        path: args.data.join("admin.sock"),
        client: None,
        json: args.json,
        stdin,
        out: stdout,
    };
    match args.noun {
        Noun::User(cmd) => users::run(cmd, &mut cx).await,
        Noun::Token(cmd) => users::run_token(cmd, &mut cx).await,
        Noun::Team(cmd) => teams::run_team(cmd, &mut cx).await,
        Noun::Workspace(cmd) => teams::run_workspace(cmd, &mut cx).await,
        Noun::Grant(cmd) => access::run_grant(cmd, &mut cx).await,
        Noun::Override(cmd) => access::run_override(cmd, &mut cx).await,
        Noun::Update(cmd) => update::run(cmd, &mut cx).await,
    }
}

/// What every verb needs: the socket, the output mode, stdin and the writer. The client
/// connects on the first request, so a verb reads stdin before a wrong `--data` is reported.
pub(crate) struct Context<'a> {
    path: PathBuf,
    client: Option<AdminClient>,
    json: bool,
    stdin: &'a mut dyn Read,
    out: &'a mut dyn Write,
}

impl Context<'_> {
    /// Sends one request expecting a status in `ok`, then prints the body as pretty JSON or
    /// as the table `rows` renders from it. A body that is not a JSON object is `Malformed`.
    pub(crate) async fn call(
        &mut self,
        verb: Verb,
        target: &str,
        body: Option<&str>,
        ok: &[u16],
        rows: impl FnOnce(&Value) -> String,
    ) -> Result<(), AdminError> {
        let response = self.send(verb, target, body).await?;
        let status = response.status();
        let Some(value) = expect(response, ok)? else {
            return Ok(());
        };
        if !value.is_object() {
            return Err(AdminError::Malformed { status });
        }
        let text = if self.json {
            pretty(&value)?
        } else {
            rows(&value)
        };
        self.write(&text)
    }

    /// As `call`, for a list: a body that is not a JSON array is `Malformed`, and `rows`
    /// renders its elements.
    pub(crate) async fn call_list(
        &mut self,
        verb: Verb,
        target: &str,
        body: Option<&str>,
        ok: &[u16],
        rows: impl FnOnce(&[Value]) -> String,
    ) -> Result<(), AdminError> {
        let response = self.send(verb, target, body).await?;
        let status = response.status();
        let Some(value) = expect(response, ok)? else {
            return Ok(());
        };
        let Some(items) = value.as_array() else {
            return Err(AdminError::Malformed { status });
        };
        let text = if self.json {
            pretty(&value)?
        } else {
            rows(items)
        };
        self.write(&text)
    }

    /// Sends one request expecting a status in `ok` and returns its JSON body, if any.
    pub(crate) async fn fetch(
        &mut self,
        verb: Verb,
        target: &str,
        body: Option<&str>,
        ok: &[u16],
    ) -> Result<Option<Value>, AdminError> {
        expect(self.send(verb, target, body).await?, ok)
    }

    /// Connects on first use, then sends one request and returns the raw response.
    async fn send(
        &mut self,
        verb: Verb,
        target: &str,
        body: Option<&str>,
    ) -> Result<ApiResponse, AdminError> {
        let client = match &mut self.client {
            Some(client) => client,
            None => {
                let client = AdminClient::connect(&self.path).await.map_err(|source| {
                    AdminError::Unreachable {
                        path: self.path.clone(),
                        source,
                    }
                })?;
                self.client.insert(client)
            }
        };
        client
            .send(verb, target, body)
            .await
            .map_err(|source| AdminError::Transport { source })
    }

    /// Sends one bodiless request expecting 204: a person gets `line`, JSON mode stays silent.
    pub(crate) async fn confirm(
        &mut self,
        verb: Verb,
        target: &str,
        line: &str,
    ) -> Result<(), AdminError> {
        self.fetch(verb, target, None, &[204]).await?;
        if self.json {
            return Ok(());
        }
        self.write(line)
    }

    /// The password from stdin when `from_stdin`, else a generated one.
    pub(crate) fn new_password(&mut self, from_stdin: bool) -> Result<NewPassword, AdminError> {
        NewPassword::obtain(from_stdin, self.stdin)
    }

    /// Whether `--json` was given.
    pub(crate) fn json(&self) -> bool {
        self.json
    }

    /// Writes `text` to stdout.
    pub(crate) fn write(&mut self, text: &str) -> Result<(), AdminError> {
        self.out
            .write_all(text.as_bytes())
            .map_err(AdminError::Output)
    }
}

/// `value` checked as a name before it becomes one path segment: the URL parser would resolve
/// `..`, `%2e%2e` and `\`, and split at `?` or `#`, so anything else could reach another route.
pub(crate) fn segment(arg: &'static str, value: &str) -> Result<Slug, AdminError> {
    Slug::parse(value).map_err(|reason| AdminError::BadName {
        arg,
        value: value.to_owned(),
        reason,
    })
}

/// `value` as pretty JSON with a final newline.
pub(crate) fn pretty(value: &Value) -> Result<String, AdminError> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|e| AdminError::Output(io::Error::other(e)))?;
    text.push('\n');
    Ok(text)
}

/// A password for a new or reset account, and whether this command made it up.
pub(crate) struct NewPassword {
    pub(crate) text: String,
    pub(crate) generated: bool,
}

impl NewPassword {
    /// The password from stdin when `from_stdin`, else a generated one.
    pub(crate) fn obtain(from_stdin: bool, stdin: &mut dyn Read) -> Result<Self, AdminError> {
        Ok(if from_stdin {
            NewPassword {
                text: password_from_stdin(stdin)?,
                generated: false,
            }
        } else {
            NewPassword {
                text: generate_password()?,
                generated: true,
            }
        })
    }
}

/// 24 bytes from `getrandom::fill`, base64url without padding: 32 characters.
pub(crate) fn generate_password() -> Result<String, AdminError> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| AdminError::Rng)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// All of stdin as UTF-8 (else `Stdin` with `InvalidData`), minus one trailing `\n` and a `\r`
/// before it; nothing else is trimmed.
pub(crate) fn password_from_stdin(stdin: &mut dyn Read) -> Result<String, AdminError> {
    let mut bytes = Vec::new();
    stdin.read_to_end(&mut bytes).map_err(AdminError::Stdin)?;
    let mut text = String::from_utf8(bytes)
        .map_err(|_| AdminError::Stdin(io::Error::from(io::ErrorKind::InvalidData)))?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// `Ok(Some(body))` when `response.status()` is in `ok` and carries JSON, `Ok(None)` for an
/// expected 204; an expected status with an empty or non-JSON body is `Malformed`. A non-2xx
/// whose body is the error model is `Api`; any other status is `Malformed`.
pub(crate) fn expect(response: ApiResponse, ok: &[u16]) -> Result<Option<Value>, AdminError> {
    let status = response.status();
    if ok.contains(&status) {
        // Only 204 is bodiless by contract; any other expected status must carry JSON.
        if status == 204 {
            return Ok(None);
        }
        return response
            .json()
            .map(Some)
            .ok_or(AdminError::Malformed { status });
    }
    let error = (!(200..300).contains(&status))
        .then(|| response.json())
        .flatten()
        .and_then(|body| {
            let error = body.get("error")?;
            Some(AdminError::Api {
                status,
                code: error.get("code")?.as_str()?.to_owned(),
                message: error.get("message")?.as_str()?.to_owned(),
                field: error
                    .get("field")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        });
    Err(error.unwrap_or(AdminError::Malformed { status }))
}
