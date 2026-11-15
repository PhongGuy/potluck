//! The potluck admin command line over the admin socket.

pub mod client;
pub mod output;
pub mod users;

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde_json::Value;

use self::client::{AdminClient, ApiResponse, Verb};

/// `potluck admin`: one API call per verb, as the `system` actor.
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct AdminArgs {
    /// The running server's data directory, where `admin.sock` lives.
    #[arg(long, global = true, env = "POTLUCK_DATA", default_value = "./data")]
    data: PathBuf,
    /// Print the server's JSON answer instead of a table.
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
    /// A status the verb did not expect, without the error model's body.
    #[error("the server answered HTTP {status} without an error body")]
    Malformed {
        /// The HTTP status.
        status: u16,
    },
    /// Writing to stdout failed.
    #[error("cannot write the output")]
    Output(#[source] io::Error),
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
    // No verb reads stdin yet; `user create --password-stdin` will.
    let _ = stdin;
    let path = args.data.join("admin.sock");
    let client = AdminClient::connect(&path)
        .await
        .map_err(|source| AdminError::Unreachable { path, source })?;
    let mut cx = Context {
        client,
        json: args.json,
        out: stdout,
    };
    match args.noun {
        Noun::User(cmd) => users::run(cmd, &mut cx).await,
    }
}

/// What every verb needs: the client, the output mode and the writer.
pub(crate) struct Context<'a> {
    client: AdminClient,
    json: bool,
    out: &'a mut dyn Write,
}

impl Context<'_> {
    /// Sends one request expecting a status in `ok`, then prints the body as pretty JSON or
    /// as the table `rows` renders from it.
    pub(crate) async fn call(
        &mut self,
        verb: Verb,
        target: &str,
        body: Option<&str>,
        ok: &[u16],
        rows: impl FnOnce(&Value) -> String,
    ) -> Result<(), AdminError> {
        let response = self
            .client
            .send(verb, target, body)
            .await
            .map_err(|source| AdminError::Transport { source })?;
        let Some(value) = expect(response, ok)? else {
            return Ok(());
        };
        let text = if self.json {
            let mut pretty = serde_json::to_string_pretty(&value)
                .map_err(|e| AdminError::Output(io::Error::other(e)))?;
            pretty.push('\n');
            pretty
        } else {
            rows(&value)
        };
        self.out
            .write_all(text.as_bytes())
            .map_err(AdminError::Output)
    }
}

/// `Ok(body)` when `response.status()` is in `ok`; a non-2xx whose body is the error model is
/// `Api`; any other status is `Malformed`.
pub(crate) fn expect(response: ApiResponse, ok: &[u16]) -> Result<Option<Value>, AdminError> {
    let status = response.status();
    if ok.contains(&status) {
        return Ok(response.json());
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
