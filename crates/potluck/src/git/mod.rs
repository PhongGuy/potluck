//! Git smart HTTP: the bare repositories and the git processes that serve them.

pub mod repos;

pub use repos::{RepoError, Repos};
