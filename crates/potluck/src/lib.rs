//! Potluck: a self-hosted team server for local-first desktop apps.

pub mod api;
pub mod auth;
pub mod cli;
pub mod clock;
pub mod config;
pub mod db;
pub mod git;
pub mod health_check;
pub mod hooks;
pub mod role;
pub mod server;
pub mod slug;
mod unix_http;
