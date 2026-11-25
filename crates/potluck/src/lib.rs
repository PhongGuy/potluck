//! Potluck: a self-hosted team server for local-first desktop apps.

pub mod admin;
pub mod api;
pub mod auth;
pub mod backup;
pub mod cli;
pub mod clock;
pub mod config;
pub mod db;
pub mod git;
pub mod health_check;
pub mod hooks;
mod join_failure;
pub mod kinds;
mod metrics;
pub mod notices;
pub mod oidc;
pub(crate) mod retention;
pub mod role;
pub mod secret_check;
pub mod server;
pub mod slug;
mod unicode;
pub mod update;
