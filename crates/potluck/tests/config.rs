//! `potluck serve` flags become a validated `Config`. Every test passes its flags
//! explicitly, since clap also reads `POTLUCK_*` from the environment; a shell that
//! exports `POTLUCK_PUBLIC_URL` makes `public_url_is_required` fail.

use std::net::SocketAddr;
use std::path::Path;

use clap::Parser;
use potluck::config::ConfigError;

use crate::common::{config_for, config_listening_on, parse_config};

#[test]
fn defaults_match_the_spec() {
    let config = parse_config(&["--public-url", "http://127.0.0.1:8080"]).unwrap();
    assert_eq!(
        config.listen(),
        "127.0.0.1:8080".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(
        config.data(),
        std::path::absolute("./data").unwrap(),
        "data is stored absolute"
    );
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
    assert_eq!(config.lock_path(), Path::new("/srv/potluck/server.lock"));
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
fn credentials_in_an_unparsable_public_url_are_not_echoed() {
    let err = parse_config(&["--public-url", "https://alice:pw@host:99999"]).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidPublicUrl { .. }),
        "got {err:?}"
    );
    let text = format!("{err} {err:?}");
    assert!(!text.contains("alice"), "the username leaked: {text}");
    assert!(!text.contains("pw"), "the password leaked: {text}");
}

#[test]
fn credentials_are_not_echoed_whichever_check_refuses_the_url() {
    for (url, password) in [
        ("https://alice:p/w@host", "p/w"),
        ("https://alice:p?w@host", "p?w"),
        ("https://alice:p#w@host", "p#w"),
        ("//alice:pw@host", "pw"),
        ("alice:pw@host", "pw"),
        ("alice:pw@x://host", "pw"),
    ] {
        let err = parse_config(&["--public-url", url]).unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(
            !text.contains("alice"),
            "{url}: the username leaked: {text}"
        );
        assert!(
            !text.contains(password),
            "{url}: the password leaked: {text}"
        );
    }
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

#[test]
fn data_is_made_absolute_and_tmp_lives_under_it() {
    let config = parse_config(&[
        "--data",
        "rel/data",
        "--public-url",
        "https://p.example.com",
    ])
    .unwrap();
    assert!(config.data().is_absolute(), "{}", config.data().display());
    assert!(
        config.data().ends_with("rel/data"),
        "{}",
        config.data().display()
    );
    assert_eq!(config.hooks_dir(), config.data().join("hooks"));
    assert_eq!(config.tmp_dir(), config.data().join("tmp"));
    assert!(config.hooks_dir().is_absolute());
    let config = parse_config(&[
        "--data",
        "/srv/potluck",
        "--public-url",
        "https://p.example.com",
    ])
    .unwrap();
    assert_eq!(config.tmp_dir(), Path::new("/srv/potluck/tmp"));
}

#[test]
fn trusted_proxies_parse_cidrs_and_bare_addresses() {
    let config = parse_config(&[
        "--public-url",
        "https://p.example.com",
        "--trusted-proxies",
        "10.0.0.0/8,192.168.1.5, fd00::/8,",
    ])
    .unwrap();
    assert!(config.trusts_proxy("10.1.2.3".parse().unwrap()));
    assert!(
        config.trusts_proxy("192.168.1.5".parse().unwrap()),
        "a bare address is itself"
    );
    assert!(!config.trusts_proxy("192.168.1.6".parse().unwrap()));
    assert!(
        config.trusts_proxy("fd00::1".parse().unwrap()),
        "spaces around entries are trimmed"
    );
    assert!(!config.trusts_proxy("127.0.0.1".parse().unwrap()));
}

#[test]
fn an_ipv4_mapped_ipv6_peer_matches_ipv4_cidrs() {
    let config = parse_config(&[
        "--public-url",
        "https://p.example.com",
        "--trusted-proxies",
        "10.0.0.0/8",
    ])
    .unwrap();
    assert!(config.trusts_proxy("::ffff:10.1.2.3".parse().unwrap()));
    assert!(!config.trusts_proxy("::ffff:192.168.1.1".parse().unwrap()));
    assert!(!config.trusts_proxy("fd00::1".parse().unwrap()));
}

#[test]
fn no_proxy_is_trusted_by_default() {
    let config = parse_config(&["--public-url", "https://p.example.com"]).unwrap();
    assert!(!config.trusts_proxy("127.0.0.1".parse().unwrap()));
    assert!(!config.trusts_proxy("10.0.0.1".parse().unwrap()));
}

#[test]
fn a_bad_trusted_proxy_entry_is_refused() {
    for bad in ["10.0.0.0/33", "proxy.example.com", "10.0.0/8", "fd00::/129"] {
        let err = parse_config(&[
            "--public-url",
            "https://p.example.com",
            "--trusted-proxies",
            bad,
        ])
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidTrustedProxy { .. }),
            "{bad}: got {err:?}"
        );
        assert!(err.to_string().contains(bad), "{err}");
    }
}

#[test]
fn oidc_flags_parse_but_are_refused_while_unimplemented() {
    for flags in [
        &["--oidc-issuer", "https://issuer.example.com"][..],
        &["--oidc-client-id", "potluck"][..],
        &["--oidc-client-secret-file", "/run/secrets/oidc"][..],
    ] {
        let mut args = vec!["--public-url", "https://p.example.com"];
        args.extend_from_slice(flags);
        let err = parse_config(&args).unwrap_err();
        assert!(
            matches!(err, ConfigError::OidcUnsupported),
            "{flags:?}: got {err:?}"
        );
        assert!(err.to_string().contains("--oidc-issuer"), "{err}");
    }
}

#[test]
fn config_listening_on_takes_the_address_and_config_for_asks_for_port_zero() {
    let dir = tempfile::tempdir().unwrap();
    let addr: SocketAddr = "127.0.0.1:4321".parse().unwrap();
    let config = config_listening_on(dir.path(), addr);
    assert_eq!(config.listen(), addr);
    assert_eq!(config.db_path(), dir.path().join("server.db"));
    assert_eq!(config_for(dir.path()).listen().port(), 0);
}

#[test]
fn the_git_run_limit_defaults_to_thirty_minutes_and_is_read() {
    let config = parse_config(&["--public-url", "http://127.0.0.1:8080"]).unwrap();
    assert_eq!(
        config.git_max_runtime(),
        std::time::Duration::from_secs(1800)
    );
    let config = parse_config(&[
        "--public-url",
        "http://127.0.0.1:8080",
        "--git-max-runtime-secs",
        "90",
    ])
    .unwrap();
    assert_eq!(config.git_max_runtime(), std::time::Duration::from_secs(90));
}

#[test]
fn a_zero_git_run_limit_is_refused() {
    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        serve: potluck::config::ServeArgs,
    }
    let err = Wrap::try_parse_from([
        "potluck",
        "--public-url",
        "http://127.0.0.1:8080",
        "--git-max-runtime-secs",
        "0",
    ])
    .err()
    .expect("zero is refused");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}

#[test]
fn serve_help_lists_the_git_run_limit() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_potluck"))
        .args(["serve", "--help"])
        .env_clear()
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "--git-max-runtime-secs",
        "POTLUCK_GIT_MAX_RUNTIME_SECS",
        "1800",
    ] {
        assert!(text.contains(needle), "help lacks {needle}: {text}");
    }
}
