//! The potluck integration suite: one binary, every test file declared here as a module.
//! A new test file must be added below, or it never runs.

// Test helpers panic by design and each assert is named by its test; the stricter forms
// only matter in shipped code.
#![allow(
    clippy::expect_used,
    clippy::missing_assert_message,
    clippy::unwrap_used
)]

mod common;

mod access;
mod actor;
mod api_error;
mod api_json;
mod api_system;
mod auth;
mod authenticate;
mod cli_version;
mod client_ip;
mod config;
mod credential;
mod db;
mod db_tokens;
mod db_users;
mod git_denials;
mod git_http;
mod git_requests;
mod health_check;
mod hooks;
mod measurements;
mod objects;
mod password;
mod rate_limit;
mod repos;
mod serve;
mod session;
mod session_proxies;
mod slug;
mod token;
mod tokens;
mod users;
mod users_patch;
