//! The `potluck serve` flags and the validated [`Config`] built from them.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// Longest a git process (and its hook) may run before it is killed, in seconds.
    #[arg(long, env = "POTLUCK_GIT_MAX_RUNTIME_SECS",
          default_value_t = crate::git::engine::DEFAULT_MAX_RUNTIME_SECS,
          value_parser = clap::value_parser!(u64).range(1..))]
    git_max_runtime_secs: u64,
    /// Tracing filter directive, such as `info` or `potluck=debug`.
    #[arg(long, env = "POTLUCK_LOG", default_value = "info")]
    log: String,
    /// CIDRs or addresses whose `X-Forwarded-For` is believed; comma separated, none by default;
    /// an IPv4-mapped IPv6 range counts as the IPv4 range it maps.
    #[arg(long, env = "POTLUCK_TRUSTED_PROXIES", value_delimiter = ',')]
    trusted_proxies: Vec<String>,
    /// OIDC issuer URL; parsed, but refused until OIDC sign-in exists.
    #[arg(long, env = "POTLUCK_OIDC_ISSUER")]
    oidc_issuer: Option<String>,
    /// OIDC client id; parsed, but refused until OIDC sign-in exists.
    #[arg(long, env = "POTLUCK_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,
    /// File holding the OIDC client secret; parsed, but refused until OIDC sign-in exists.
    #[arg(long, env = "POTLUCK_OIDC_CLIENT_SECRET_FILE")]
    oidc_client_secret_file: Option<PathBuf>,
    /// Turn off the daily check for a new signed release.
    #[arg(long = "no-update-check", env = "POTLUCK_UPDATE_CHECK", action = clap::ArgAction::SetFalse)]
    update_check: bool,
    /// Days an audit row of a change is kept; 0 keeps them forever.
    #[arg(long, env = "POTLUCK_AUDIT_RETENTION_DAYS", default_value_t = 365)]
    audit_retention_days: u32,
    /// Days an audit row of a failed sign-in is kept; 0 keeps them forever.
    #[arg(
        long,
        env = "POTLUCK_AUDIT_FAILED_RETENTION_DAYS",
        default_value_t = 30
    )]
    audit_failed_retention_days: u32,
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
    git_max_runtime: Duration,
    log: String,
    trusted_proxies: Vec<ipnet::IpNet>,
    update_check: bool,
    audit_retention_days: u32,
    audit_failed_retention_days: u32,
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

    /// How long a git child may run; `--git-max-runtime-secs`.
    pub fn git_max_runtime(&self) -> Duration {
        self.git_max_runtime
    }

    /// The tracing filter directive.
    pub fn log(&self) -> &str {
        &self.log
    }

    /// Whether the daily release check runs; off with `--no-update-check`.
    pub fn update_check(&self) -> bool {
        self.update_check
    }

    /// Days an audit row of a change is kept; 0 keeps change rows forever.
    pub fn audit_retention_days(&self) -> u32 {
        self.audit_retention_days
    }

    /// Days a `session.failed` audit row is kept; 0 keeps those rows forever.
    pub fn audit_failed_retention_days(&self) -> u32 {
        self.audit_failed_retention_days
    }

    /// Whether `ip` is inside one of `--trusted-proxies`; an IPv4-mapped IPv6 address is tested
    /// as its IPv4 form too.
    pub fn trusts_proxy(&self, ip: IpAddr) -> bool {
        let unmapped = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4),
            IpAddr::V4(_) => None,
        };
        self.trusted_proxies
            .iter()
            .any(|net| net.contains(&ip) || unmapped.is_some_and(|v4| net.contains(&v4)))
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

    /// Refuses a `<data>/admin.sock` too long to bind as a Unix socket, with
    /// [`ConfigError::AdminSocketPathTooLong`].
    pub fn check_admin_socket(&self) -> Result<(), ConfigError> {
        // Measured as given, `..` segments included, because that is the path bind sees.
        let path = self.admin_socket();
        let len = path.as_os_str().len();
        if len > MAX_ADMIN_SOCKET_LEN {
            return Err(ConfigError::AdminSocketPathTooLong {
                path,
                len,
                max: MAX_ADMIN_SOCKET_LEN,
            });
        }
        Ok(())
    }

    /// `<data>/server.lock`: the advisory lock `serve` holds while it runs.
    pub fn lock_path(&self) -> PathBuf {
        self.data.join("server.lock")
    }

    /// `<data>/tmp`: where request bodies are spooled.
    pub fn tmp_dir(&self) -> PathBuf {
        self.data.join("tmp")
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
    /// `--data` could not be made absolute (the working directory is unreadable).
    #[error("--data {path} cannot be made absolute: {reason}")]
    DataDir {
        /// The directory as given.
        path: PathBuf,
        /// The io error's text.
        reason: String,
    },
    /// A `--trusted-proxies` entry is neither an address nor a CIDR.
    #[error("--trusted-proxies entry {value:?} is not an IP address or CIDR: {reason}")]
    InvalidTrustedProxy {
        /// The entry as given.
        value: String,
        /// The parser's complaint.
        reason: String,
    },
    /// An `--oidc-*` flag is set, and OIDC is not implemented yet.
    #[error(
        "OIDC sign-in is not implemented in this version; unset --oidc-issuer, --oidc-client-id and --oidc-client-secret-file (POTLUCK_OIDC_*)"
    )]
    OidcUnsupported,
    /// `<data>/admin.sock` is too long to bind as a Unix socket.
    #[error(
        "the admin socket {path} is {len} bytes, over the {max}-byte limit for a Unix socket path; choose a shorter --data"
    )]
    AdminSocketPathTooLong {
        /// `<data>/admin.sock`, with `--data` made absolute.
        path: PathBuf,
        /// Its length in bytes.
        len: usize,
        /// The longest accepted length in bytes (103 on Linux).
        max: usize,
    },
}

/// Size of `sockaddr_un.sun_path` on Linux (`UNIX_PATH_MAX` in `linux/un.h`).
const SUN_PATH_LEN: usize = 108;
/// The extra bytes of the temporary name `server::bind_admin_socket` binds first.
const ADMIN_SOCKET_TMP_SUFFIX_LEN: usize = ".tmp".len();
/// The longest `<data>/admin.sock` that binds: one byte of `sun_path` is the NUL, and the
/// temporary name must fit too.
const MAX_ADMIN_SOCKET_LEN: usize = SUN_PATH_LEN - 1 - ADMIN_SOCKET_TMP_SUFFIX_LEN;

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
        // Absolute, so git resolves `core.hooksPath` and repository paths the same way
        // whatever directory it runs in.
        let data = std::path::absolute(&args.data).map_err(|e| ConfigError::DataDir {
            path: args.data.clone(),
            reason: e.to_string(),
        })?;
        let trusted_proxies = parse_trusted_proxies(&args.trusted_proxies)?;
        if args.oidc_issuer.is_some()
            || args.oidc_client_id.is_some()
            || args.oidc_client_secret_file.is_some()
        {
            return Err(ConfigError::OidcUnsupported);
        }
        Ok(Self {
            listen: args.listen,
            data,
            public_url,
            git: args.git,
            max_push_bytes: args.max_push_bytes,
            git_max_runtime: Duration::from_secs(args.git_max_runtime_secs),
            log: args.log,
            trusted_proxies,
            update_check: args.update_check,
            audit_retention_days: args.audit_retention_days,
            audit_failed_retention_days: args.audit_failed_retention_days,
        })
    }
}

/// Each entry trimmed, empty ones dropped (so a trailing comma is harmless); a bare address
/// becomes the single-host net it names, anything else must parse as a CIDR. A net inside
/// `::ffff:0:0/96` is returned as the IPv4 net it maps.
fn parse_trusted_proxies(entries: &[String]) -> Result<Vec<ipnet::IpNet>, ConfigError> {
    entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            if let Ok(addr) = entry.parse::<IpAddr>() {
                return Ok(ipnet::IpNet::from(addr));
            }
            entry
                .parse::<ipnet::IpNet>()
                .map_err(|e| ConfigError::InvalidTrustedProxy {
                    value: entry.to_owned(),
                    reason: e.to_string(),
                })
        })
        .map(|net| net.map(unmapped))
        .collect()
}

/// Peers reach `trusts_proxy` already unmapped, so a range wholly inside the mapped block is
/// stored as its IPv4 net. A wider IPv6 range stays IPv6-only and never trusts IPv4 peers.
fn unmapped(net: ipnet::IpNet) -> ipnet::IpNet {
    let ipnet::IpNet::V6(v6) = net else {
        return net;
    };
    match v6.network().to_ipv4_mapped() {
        Some(v4) if v6.prefix_len() >= 96 => ipnet::Ipv4Net::new(v4, v6.prefix_len() - 96)
            .map(ipnet::IpNet::V4)
            .unwrap_or(net),
        _ => net,
    }
}

/// Refuses a public URL that is malformed, carries credentials, a query or a fragment, is
/// not http(s), or would send tokens in clear over the network without the operator opting
/// in. Returns the parsed URL without a trailing slash.
fn validate_public_url(raw: &str, insecure_http: bool) -> Result<String, ConfigError> {
    let invalid = |reason: String| ConfigError::InvalidPublicUrl {
        url: redact_userinfo(raw),
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
                url: redact_userinfo(raw),
            });
        }
        _ => {
            return Err(ConfigError::UnsupportedScheme {
                url: redact_userinfo(raw),
            });
        }
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The input with everything from after its first `://` (or its start) up to its last
/// `@` replaced by `***`, so error text never carries credentials, parsed or not.
fn redact_userinfo(raw: &str) -> String {
    let Some(at) = raw.rfind('@') else {
        return raw.to_owned();
    };
    let start = raw[..at].find("://").map_or(0, |i| i + 3);
    format!("{}***{}", &raw[..start], &raw[at..])
}

fn is_loopback(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(name)) => name == "localhost",
        None => false,
    }
}
