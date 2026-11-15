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
mod api_error;
mod api_system;
mod cli_version;
mod config;
mod credential;
mod db;
mod git_denials;
mod git_http;
mod git_requests;
mod health_check;
mod hooks;
mod measurements;
mod password;
mod repos;
mod serve;
mod slug;
mod token;
