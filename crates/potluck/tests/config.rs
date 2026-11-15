//! `potluck serve` flags become a validated `Config`. Every test passes its flags
//! explicitly, since clap also reads `POTLUCK_*` from the environment; a shell that
//! exports `POTLUCK_PUBLIC_URL` makes `public_url_is_required` fail.

use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use potluck::config::ConfigError;

use crate::common::parse_config;

#[test]
fn defaults_match_the_spec() {
    let config = parse_config(&["--public-url", "http://127.0.0.1:8080"]).unwrap();
    assert_eq!(
        config.listen(),
        "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(config.data(), Path::new("./data"));
    assert_eq!(config.git(), Path::new("git"));
    assert_eq!(config.max_push_bytes(), 100 * 1024 * 1024);
    assert_eq!(config.log(), "info");
    assert_eq!(config.public_url(), "http://127.0.0.1:8080");
}

#[test]
fn derived_paths_live_under_data() {
    let config = parse_config(&[
        "--data",
        "/srv/potluck",
        "--public-url",
        "https://p.example.com",
    ])
    .unwrap();
    assert_eq!(config.db_path(), Path::new("/srv/potluck/server.db"));
    assert_eq!(config.repos_dir(), Path::new("/srv/potluck/repos"));
    assert_eq!(config.hooks_dir(), Path::new("/srv/potluck/hooks"));
    assert_eq!(config.admin_socket(), Path::new("/srv/potluck/admin.sock"));
}

#[test]
fn every_flag_is_read() {
    let config = parse_config(&[
        "--listen",
        "0.0.0.0:9000",
        "--data",
        "/tmp/x",
        "--public-url",
        "https://p.example.com",
        "--git",
        "/usr/bin/git",
        "--max-push-bytes",
        "1024",
        "--log",
        "debug",
    ])
    .unwrap();
    assert_eq!(
        config.listen(),
        "0.0.0.0:9000".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(config.git(), Path::new("/usr/bin/git"));
    assert_eq!(config.max_push_bytes(), 1024);
    assert_eq!(config.log(), "debug");
}

#[test]
fn loopback_http_is_allowed() {
    for url in [
        "http://127.0.0.1:8080",
        "http://localhost:8080",
        "http://[::1]:8080",
        "http://127.5.5.5",
    ] {
        parse_config(&["--public-url", url]).unwrap_or_else(|e| panic!("{url}: {e}"));
    }
}

#[test]
fn non_loopback_http_is_refused_without_insecure_http() {
    let err = parse_config(&["--public-url", "http://team.example.com"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::InsecurePublicUrl { .. }),
        "got {err:?}"
    );
    assert!(
        err.to_string().contains("--insecure-http"),
        "the message names the escape hatch: {err}"
    );
    parse_config(&["--public-url", "http://team.example.com", "--insecure-http"]).unwrap();
}

#[test]
fn https_is_allowed_on_any_host() {
    parse_config(&["--public-url", "https://team.example.com"]).unwrap();
}

#[test]
fn other_schemes_are_refused() {
    let err = parse_config(&["--public-url", "ftp://team.example.com"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::UnsupportedScheme { .. }),
        "got {err:?}"
    );
}

#[test]
fn malformed_urls_and_queries_are_refused() {
    let err = parse_config(&["--public-url", "not a url"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidPublicUrl { .. }),
        "got {err:?}"
    );
    let err = parse_config(&["--public-url", "https://p.example.com/?x=1"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidPublicUrl { .. }),
        "got {err:?}"
    );
}

#[test]
fn trailing_slashes_are_trimmed_and_a_path_prefix_is_kept() {
    let config = parse_config(&["--public-url", "https://p.example.com/"]).unwrap();
    assert_eq!(config.public_url(), "https://p.example.com");
    let config = parse_config(&["--public-url", "https://p.example.com/potluck/"]).unwrap();
    assert_eq!(config.public_url(), "https://p.example.com/potluck");
}

#[test]
fn trailing_whitespace_is_not_stored() {
    let config = parse_config(&["--public-url", "https://p.example.com/potluck "]).unwrap();
    assert_eq!(config.public_url(), "https://p.example.com/potluck");
}

#[test]
fn credentials_in_the_public_url_are_refused_and_not_echoed() {
    let err = parse_config(&["--public-url", "https://alice:s3cretpw@p.example.com"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::PublicUrlHasCredentials),
        "got {err:?}"
    );
    let text = format!("{err} {err:?}");
    assert!(!text.contains("alice"), "the username leaked: {text}");
    assert!(!text.contains("s3cretpw"), "the password leaked: {text}");
}

#[test]
fn a_username_alone_in_the_public_url_is_refused() {
    let err = parse_config(&["--public-url", "http://alice@team.example.com"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::PublicUrlHasCredentials),
        "got {err:?}"
    );
}

#[test]
fn an_invalid_log_filter_is_refused() {
    let err = parse_config(&[
        "--public-url",
        "https://p.example.com",
        "--log",
        "nope=notalevel",
    ])
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidLog { .. }), "got {err:?}");
}

#[test]
fn public_url_is_required() {
    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        serve: potluck::config::ServeArgs,
    }
    let err = Wrap::try_parse_from(["potluck"])
        .err()
        .expect("missing --public-url is an error");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}
