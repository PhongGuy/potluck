//! The `potluck serve` flags and the validated [`Config`] built from them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use url::{Host, Url};

/// Flags of `potluck serve`; each has a `POTLUCK_*` environment variable.
#[derive(clap::Args, Debug, Clone)]
#[non_exhaustive]
pub struct ServeArgs {
    /// TCP address to listen on.
    #[arg(long, env = "POTLUCK_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Data directory for the database, repositories, hooks and the admin socket.
    #[arg(long, env = "POTLUCK_DATA", default_value = "./data")]
    data: PathBuf,
    /// The URL clients reach the server at, including any path prefix.
    #[arg(long, env = "POTLUCK_PUBLIC_URL")]
    public_url: String,
    /// Accept a plain http public URL on a non-loopback host.
    #[arg(long, env = "POTLUCK_INSECURE_HTTP", action = clap::ArgAction::SetTrue)]
    insecure_http: bool,
    /// The git binary to spawn.
    #[arg(long, env = "POTLUCK_GIT", default_value = "git")]
    git: PathBuf,
    /// Largest accepted push body, in bytes.
    #[arg(long, env = "POTLUCK_MAX_PUSH_BYTES", default_value_t = 100 * 1024 * 1024)]
    max_push_bytes: u64,
    /// Tracing filter directive, such as `info` or `potluck=debug`.
    #[arg(long, env = "POTLUCK_LOG", default_value = "info")]
    log: String,
}

/// Validated server configuration. Built with `Config::try_from(ServeArgs)`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Config {
    listen: SocketAddr,
    data: PathBuf,
    public_url: String,
    git: PathBuf,
    max_push_bytes: u64,
    log: String,
}

impl Config {
    /// The TCP address `serve` binds.
    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// The data directory holding the database, repositories, hooks and the admin socket.
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// Scheme, host, port and path prefix, without a trailing slash.
    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// The git binary to spawn.
    pub fn git(&self) -> &Path {
        &self.git
    }

    /// The push body cap in bytes.
    pub fn max_push_bytes(&self) -> u64 {
        self.max_push_bytes
    }

    /// The tracing filter directive.
    pub fn log(&self) -> &str {
        &self.log
    }

    /// `<data>/server.db`.
    pub fn db_path(&self) -> PathBuf {
        self.data.join("server.db")
    }

    /// `<data>/repos`.
    pub fn repos_dir(&self) -> PathBuf {
        self.data.join("repos")
    }

    /// `<data>/hooks`.
    pub fn hooks_dir(&self) -> PathBuf {
        self.data.join("hooks")
    }

    /// `<data>/admin.sock`.
    pub fn admin_socket(&self) -> PathBuf {
        self.data.join("admin.sock")
    }
}

/// Why `serve` flags were refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The public URL does not parse, or carries a query or fragment.
    #[error("--public-url {url:?} is not a valid URL: {reason}")]
    InvalidPublicUrl {
        /// The URL as given.
        url: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The public URL carries a username or password. The message leaves the URL out so
    /// the credentials never reach a log.
    #[error("--public-url must not carry a username or password")]
    PublicUrlHasCredentials,
    /// The public URL uses a scheme other than http or https.
    #[error("--public-url {url:?} must use http or https")]
    UnsupportedScheme {
        /// The URL as given.
        url: String,
    },
    /// The public URL is plain http to a non-loopback host and `--insecure-http` is unset.
    #[error(
        "--public-url {url:?} is plain http to a non-loopback host, so tokens would cross the network in clear; put a TLS proxy in front, or pass --insecure-http to accept that"
    )]
    InsecurePublicUrl {
        /// The URL as given.
        url: String,
    },
    /// The log directive is not a valid tracing filter.
    #[error("--log {filter:?} is not a valid tracing filter: {reason}")]
    InvalidLog {
        /// The directive as given.
        filter: String,
        /// The parser's complaint.
        reason: String,
    },
}

impl TryFrom<ServeArgs> for Config {
    type Error = ConfigError;

    fn try_from(args: ServeArgs) -> Result<Self, Self::Error> {
        let public_url = validate_public_url(&args.public_url, args.insecure_http)?;
        if let Err(e) = tracing_subscriber::EnvFilter::try_new(&args.log) {
            return Err(ConfigError::InvalidLog {
                filter: args.log,
                reason: e.to_string(),
            });
        }
        Ok(Self {
            listen: args.listen,
            data: args.data,
            public_url,
            git: args.git,
            max_push_bytes: args.max_push_bytes,
            log: args.log,
        })
    }
}

/// Refuses a public URL that is malformed, carries credentials, a query or a fragment, is
/// not http(s), or would send tokens in clear over the network without the operator opting
/// in. Returns the parsed URL without a trailing slash.
fn validate_public_url(raw: &str, insecure_http: bool) -> Result<String, ConfigError> {
    let invalid = |reason: String| ConfigError::InvalidPublicUrl {
        url: raw.to_owned(),
        reason,
    };
    let url = Url::parse(raw).map_err(|e| invalid(e.to_string()))?;
    // First, because every later refusal echoes the URL it was given.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::PublicUrlHasCredentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("must not have a query or fragment".to_owned()));
    }
    match url.scheme() {
        "https" => {}
        "http" if insecure_http || is_loopback(url.host()) => {}
        "http" => {
            return Err(ConfigError::InsecurePublicUrl {
                url: raw.to_owned(),
            });
        }
        _ => {
            return Err(ConfigError::UnsupportedScheme {
                url: raw.to_owned(),
            });
        }
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn is_loopback(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(name)) => name == "localhost",
        None => false,
    }
}
