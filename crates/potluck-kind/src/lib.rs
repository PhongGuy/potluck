//! The contract between Potluck and an app's workspace rules.
//!
//! An app implements [`RuleSet`] for its workspace kind; Potluck drives it from the pre-receive
//! check, feeding it each changed path as a [`Change`] and refusing the push on any [`Finding`].
//! The crate is std-only so an app can depend on it without pulling in anything else.

#![forbid(unsafe_code)]

/// The version of this contract a rule set was built against; Potluck refuses any other.
pub const RULES_ABI: u32 = 1;

/// Why a push was refused, as the stable code carried on the wire.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    /// A `.env` file, or another file that holds secrets by its name alone.
    EnvFile,
    /// A value that must stay on the user's machine sits in a shared file.
    SecretValue,
    /// Content matches a known secret pattern such as a private key or token.
    SecretPattern,
    /// The workspace declares a format version this rule set does not support.
    FormatUnsupported,
    /// The change relaxes a safety setting the pusher's role may not relax.
    SafetyRelaxation,
}

impl Code {
    /// Returns the wire string for this code.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::EnvFile => "secret_env_file",
            Code::SecretValue => "secret_value",
            Code::SecretPattern => "secret_pattern",
            Code::FormatUnsupported => "format_unsupported",
            Code::SafetyRelaxation => "safety_relaxation",
        }
    }
}

/// The pusher's role on the workspace.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// May fetch only.
    Read,
    /// May push.
    Write,
    /// May push and administer the workspace.
    Admin,
}

/// One side of a changed file's content.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blob<'a> {
    /// The file's full bytes.
    Bytes(&'a [u8]),
    /// The file exceeds the size Potluck reads, so its bytes are not available.
    TooLarge,
}

/// One changed path handed to [`RuleSet::check`].
///
/// A history change is a blob introduced somewhere in the pushed range and has no baseline.
/// A tip change compares the path between the old and new ref tips, either side possibly absent.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change<'a> {
    path: &'a str,
    old: Option<Blob<'a>>,
    new: Option<Blob<'a>>,
    role: Role,
    baseline: bool,
}

impl<'a> Change<'a> {
    /// Builds a history change: `new` is the introduced blob, with no old side or baseline.
    pub fn history(path: &'a str, new: Blob<'a>, role: Role) -> Self {
        Change {
            path,
            old: None,
            new: Some(new),
            role,
            baseline: false,
        }
    }

    /// Builds a tip change between the old and new tips; `None` means the path is absent there.
    pub fn tip(path: &'a str, old: Option<Blob<'a>>, new: Option<Blob<'a>>, role: Role) -> Self {
        Change {
            path,
            old,
            new,
            role,
            baseline: true,
        }
    }

    /// Returns the changed path, relative to the repository root.
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// Returns whether the change compares against a baseline, which only tip changes do.
    pub fn has_baseline(&self) -> bool {
        self.baseline
    }

    /// Returns the content before the change, if there is a baseline and the path existed.
    pub fn old_blob(&self) -> Option<Blob<'a>> {
        self.old
    }

    /// Returns the content after the change, if the path still exists.
    pub fn new_blob(&self) -> Option<Blob<'a>> {
        self.new
    }

    /// Returns the pusher's role.
    pub fn role(&self) -> Role {
        self.role
    }
}

/// A reason to refuse a push, tied to one path.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Finding {
    code: Code,
    rule: &'static str,
    path: String,
    detail: Option<String>,
}

impl Finding {
    /// Builds a finding from its code, the name of the rule that raised it, and the path.
    pub fn new(code: Code, rule: &'static str, path: &str) -> Finding {
        Finding {
            code,
            rule,
            path: path.to_owned(),
            detail: None,
        }
    }

    /// Attaches a detail, such as the key inside the file; it must never carry a secret value.
    #[must_use]
    pub fn with_detail(self, detail: &str) -> Finding {
        Finding {
            detail: Some(detail.to_owned()),
            ..self
        }
    }

    /// Returns the finding's code.
    pub fn code(&self) -> Code {
        self.code
    }

    /// Returns the name of the rule that raised the finding.
    pub fn rule(&self) -> &'static str {
        self.rule
    }

    /// Returns the path the finding concerns.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the detail, if one was attached.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// A file written into a new workspace's first commit.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFile {
    path: &'static str,
    bytes: &'static [u8],
}

impl SeedFile {
    /// Builds a seed file from its repository path and content.
    pub fn new(path: &'static str, bytes: &'static [u8]) -> SeedFile {
        SeedFile { path, bytes }
    }

    /// Returns the path, relative to the repository root.
    pub fn path(&self) -> &'static str {
        self.path
    }

    /// Returns the file's content.
    pub fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

/// One workspace kind's push rules, as an app supplies them to Potluck.
///
/// Potluck holds rule sets as `&'static dyn RuleSet`, so an implementation must be object safe,
/// thread safe and free of interior state that a push could observe.
pub trait RuleSet: Send + Sync + std::fmt::Debug {
    /// Returns the kind name, such as `apinata`, as stored on the workspace.
    fn kind(&self) -> &'static str;

    /// Returns the [`RULES_ABI`] this rule set was built against.
    fn abi(&self) -> u32;

    /// Returns the workspace format versions this rule set accepts.
    fn formats(&self) -> &'static [&'static str];

    /// Returns the path of the file that declares the workspace's format version.
    fn format_file(&self) -> &'static str;

    /// Reads the format version from the format file's content, or `None` if it has none.
    fn detect_format(&self, format_file: &[u8]) -> Option<String>;

    /// Judges a path by its name alone, before any content is read.
    fn path_rule(&self, path: &str) -> Option<Finding>;

    /// Returns whether [`RuleSet::check`] needs this path's content.
    fn reads(&self, path: &str) -> bool;

    /// Judges one changed path's content and returns every finding.
    fn check(&self, change: &Change<'_>) -> Vec<Finding>;

    /// Returns the files a new workspace of this kind starts with.
    fn seed(&self) -> Vec<SeedFile>;
}
