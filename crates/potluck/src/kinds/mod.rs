//! Workspace kinds: each kind's push rules and seed files. Nothing outside this module
//! knows an app's file format.

pub mod apinata;
pub mod generic;

/// Stable refusal codes the client parses. Later rule sets add rules, never codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Code {
    /// A file named `.env`: `secret_env_file`.
    EnvFile,
    /// A `secret: true` variable carrying a value, or an environment file Apinata could not
    /// read: `secret_value`.
    SecretValue,
    /// Bytes shaped like a key or token: `secret_pattern`.
    SecretPattern,
}

impl Code {
    /// `secret_env_file`, `secret_value` or `secret_pattern`.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::EnvFile => "secret_env_file",
            Code::SecretValue => "secret_value",
            Code::SecretPattern => "secret_pattern",
        }
    }
}

/// One rule hit: what the client is told, never the matched text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Finding {
    code: Code,
    rule: &'static str,
}

impl Finding {
    /// A hit of `rule` under `code`; `rule` is a name from a rule table, never matched text.
    pub fn new(code: Code, rule: &'static str) -> Finding {
        Finding { code, rule }
    }

    /// The stable code the client parses.
    pub fn code(&self) -> Code {
        self.code
    }

    /// The rule name, such as `env_file`, `aws` or `env_unparsable`.
    pub fn rule(&self) -> &'static str {
        self.rule
    }
}

/// A file the server commits when it seeds a new workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFile {
    path: &'static str,
    bytes: &'static [u8],
}

impl SeedFile {
    /// `bytes` to be committed at `path`, relative to the workspace root.
    pub fn new(path: &'static str, bytes: &'static [u8]) -> SeedFile {
        SeedFile { path, bytes }
    }

    /// The path relative to the workspace root.
    pub fn path(&self) -> &'static str {
        self.path
    }

    /// The exact file content.
    pub fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

/// The kind module for `name`: `apinata` now; `dbento` is reserved and answers `None`.
pub fn by_name(name: &str) -> Option<&'static dyn Kind> {
    match name {
        "apinata" => Some(&apinata::Apinata),
        _ => None,
    }
}

/// A kind's own rules and seed. The generic set runs for every kind before these.
pub trait Kind: Send + Sync + std::fmt::Debug {
    /// The kind's name as `POTLUCK_KIND` and the workspace record carry it.
    fn name(&self) -> &'static str;
    /// A rule on the path alone, for every path a new commit adds or changes.
    fn path_rule(&self, path: &str) -> Option<Finding>;
    /// A rule on a candidate blob with the path it was reached by: a blob new to the
    /// repository, or any blob at a path `content_path` accepts.
    fn content_rule(&self, path: &str, blob: &[u8]) -> Option<Finding>;
    /// Whether `content_rule` applies at `path`. A blob a new commit adds or changes at such a
    /// path is a content candidate even when the repository already holds it, so a file moved
    /// or copied under the kind's paths is checked there.
    fn content_path(&self, path: &str) -> bool;
    /// The files a new workspace of this kind starts with, in commit order.
    fn seed(&self) -> Vec<SeedFile>;
}
