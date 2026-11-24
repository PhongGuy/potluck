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
mod admin_accounts;
mod admin_audit;
mod admin_cli;
mod admin_client;
mod admin_decode;
mod admin_grants;
mod admin_teams;
mod admin_update;
mod admin_users;
mod api_system;
mod audit_api;
mod audit_wiring;
mod authenticate;
mod branch_protection;
mod cli_version;
mod config;
mod credential;
mod db;
mod db_grants;
mod db_oidc;
mod db_teams;
mod db_tokens;
mod db_users;
mod db_workspaces;
mod dbento_golden;
mod deploy_files;
mod events;
mod git_access_db;
mod git_denials;
mod git_http;
mod git_push_events;
mod git_requests;
mod grants;
mod health_check;
mod hook_cli;
mod hook_push;
mod hooks;
mod kinds_generic;
mod kinds_rule_set;
mod logs;
mod measurements;
mod metrics;
mod name_checks;
mod notices;
mod objects;
mod oidc_http;
mod oidc_mock_idp;
mod oidc_provider;
mod oidc_settings;
mod oidc_store;
mod overrides;
mod panic_hook;
mod password;
mod public_api;
mod push_bookkeeping;
mod push_refs;
mod rate_limit;
mod reject_golden;
mod release_notes;
mod release_workflow;
mod repos;
mod retention;
mod rules_crates;
mod secret_entries;
mod secret_git;
mod secret_report;
mod secret_scan;
mod serve;
mod session;
mod session_proxies;
mod slug;
mod sse_reader;
mod teams;
mod test_rules;
mod third_party_notices;
mod token;
mod tokens;
mod update_api;
mod update_check;
mod update_keys;
mod update_server;
mod update_verify;
mod users;
mod users_patch;
mod workflows;
mod workspaces;
mod workspaces_read;
