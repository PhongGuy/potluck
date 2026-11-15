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
mod health_check;
mod measurements;
mod repos;
mod serve;
mod slug;
