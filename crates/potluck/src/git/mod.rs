//! Git smart HTTP: the bare repositories and the git processes that serve them.

pub mod access;
pub mod engine;
pub(crate) mod http;
pub mod push;
pub(crate) mod report;
pub mod repos;
mod spool;
mod sweep;

pub use engine::{GitEngine, GitError, GitLimits};
pub use push::RefMove;
pub use repos::{RepoError, Repos};
pub(crate) use sweep::sweep_all;

use crate::role::Role;

/// The two smart HTTP services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Service {
    /// `git-upload-pack`: clone and fetch; needs `Role::Read`.
    UploadPack,
    /// `git-receive-pack`: push; needs `Role::Write`.
    ReceivePack,
}

impl Service {
    /// `git-upload-pack` or `git-receive-pack`, as in `?service=` and the URL; `None` otherwise.
    pub fn from_name(name: &str) -> Option<Service> {
        match name {
            "git-upload-pack" => Some(Service::UploadPack),
            "git-receive-pack" => Some(Service::ReceivePack),
            _ => None,
        }
    }

    /// `git-upload-pack` / `git-receive-pack`.
    pub fn name(self) -> &'static str {
        match self {
            Service::UploadPack => "git-upload-pack",
            Service::ReceivePack => "git-receive-pack",
        }
    }

    /// `upload-pack` / `receive-pack`: the git subcommand.
    pub fn subcommand(self) -> &'static str {
        match self {
            Service::UploadPack => "upload-pack",
            Service::ReceivePack => "receive-pack",
        }
    }

    /// The least role that may use this service: `Read` to fetch, `Write` to push.
    pub fn required_role(self) -> Role {
        match self {
            Service::UploadPack => Role::Read,
            Service::ReceivePack => Role::Write,
        }
    }

    /// `application/x-git-upload-pack-advertisement` etc.
    pub fn advertisement_type(self) -> &'static str {
        match self {
            Service::UploadPack => "application/x-git-upload-pack-advertisement",
            Service::ReceivePack => "application/x-git-receive-pack-advertisement",
        }
    }

    /// `application/x-git-upload-pack-result` etc.
    pub fn result_type(self) -> &'static str {
        match self {
            Service::UploadPack => "application/x-git-upload-pack-result",
            Service::ReceivePack => "application/x-git-receive-pack-result",
        }
    }
}
