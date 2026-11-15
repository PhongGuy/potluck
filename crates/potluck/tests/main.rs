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
mod db_grants;
mod db_teams;
mod db_tokens;
mod db_users;
mod db_workspaces;
mod deploy_files;
mod git_access_db;
mod git_denials;
mod git_http;
mod git_requests;
mod grants;
mod health_check;
mod hook_cli;
mod hook_push;
mod hooks;
mod kinds_apinata;
mod kinds_generic;
mod measurements;
mod notices;
mod objects;
mod password;
mod push_refs;
mod rate_limit;
mod release_notes;
mod release_workflow;
mod repos;
mod secret_entries;
mod secret_git;
mod secret_report;
mod secret_scan;
mod serve;
mod session;
mod session_proxies;
mod slug;
mod teams;
mod token;
mod tokens;
mod users;
mod users_patch;
mod workflows;
mod workspaces;
mod workspaces_read;
