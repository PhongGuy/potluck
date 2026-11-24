//! The OIDC flags: all or none, the issuer's scheme, the secret file, the name, and a complete
//! set reaching the server's `Config`.

use std::path::{Path, PathBuf};

use potluck::config::ConfigError;
use potluck::oidc::OidcSettings;

use crate::common::parse_config;

fn secret(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("client-secret");
    std::fs::write(&path, text).unwrap();
    path
}

/// Issuer, client id and secret file of a partial set, then the expected `missing` text.
type Partial<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a Path>, &'a str);

fn full(issuer: &str, secret: &Path, insecure: bool) -> Result<Option<OidcSettings>, ConfigError> {
    OidcSettings::from_flags(
        Some(issuer),
        Some("potluck"),
        Some(secret),
        None,
        false,
        insecure,
    )
}

#[test]
fn no_flags_means_no_oidc() {
    assert!(
        OidcSettings::from_flags(None, None, None, None, false, false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_partial_set_names_the_missing_flags() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    let cases: [Partial; 5] = [
        (
            Some("https://idp.example.com"),
            None,
            None,
            "--oidc-client-id, --oidc-client-secret-file",
        ),
        (
            None,
            Some("potluck"),
            None,
            "--oidc-issuer, --oidc-client-secret-file",
        ),
        (None, None, Some(&file), "--oidc-issuer, --oidc-client-id"),
        (
            Some("https://idp.example.com"),
            Some(""),
            Some(&file),
            "--oidc-client-id",
        ),
        (
            Some("https://idp.example.com"),
            Some("  "),
            Some(&file),
            "--oidc-client-id",
        ),
    ];
    for (issuer, id, path, want) in cases {
        let err = OidcSettings::from_flags(issuer, id, path, None, false, false).unwrap_err();
        let ConfigError::OidcIncomplete { missing } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(missing, want);
        assert!(err.to_string().contains("OIDC"), "{err}");
    }
}

#[test]
fn name_or_jit_alone_is_incomplete() {
    for (name, jit) in [(Some("Acme"), false), (None, true)] {
        let err = OidcSettings::from_flags(None, None, None, name, jit, false).unwrap_err();
        assert!(matches!(err, ConfigError::OidcIncomplete { .. }), "{err:?}");
    }
}

#[test]
fn the_issuer_must_be_https_unless_loopback_or_insecure() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    for ok in [
        "https://idp.example.com",
        "http://127.0.0.1:9000",
        "http://localhost:9000",
        "http://[::1]:9000",
    ] {
        assert!(full(ok, &file, false).unwrap().is_some(), "{ok}");
    }
    let err = full("http://idp.example.com", &file, false).unwrap_err();
    assert!(
        matches!(err, ConfigError::InsecureOidcIssuer { .. }),
        "{err:?}"
    );
    assert!(
        full("http://idp.example.com", &file, true)
            .unwrap()
            .is_some()
    );
    for bad in [
        "ftp://idp.example.com",
        "not a url",
        "https://idp.example.com/?x=1",
        "https://idp.example.com/#f",
    ] {
        let err = full(bad, &file, false).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidOidcIssuer { .. }),
            "{bad}: {err:?}"
        );
    }
}

#[test]
fn an_issuer_with_credentials_is_refused_without_echoing_them() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    let err = full("https://admin:hunter2@idp.example.com", &file, false).unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidOidcIssuer { .. }),
        "{err:?}"
    );
    assert!(!err.to_string().contains("hunter2"), "{err}");
}

#[test]
fn the_issuer_is_kept_exactly_as_given() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    for issuer in [
        "https://idp.example.com/realms/acme",
        "https://idp.example.com/realms/acme/",
    ] {
        assert_eq!(
            full(issuer, &file, false).unwrap().unwrap().issuer(),
            issuer
        );
    }
}

#[test]
fn the_secret_file_loses_one_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    for (text, want) in [
        ("s3cret", "s3cret"),
        ("s3cret\n", "s3cret"),
        ("s3cret\r\n", "s3cret"),
        ("s3cret\n\n", "s3cret\n"),
    ] {
        let file = secret(dir.path(), text);
        let settings = full("https://idp.example.com", &file, false)
            .unwrap()
            .unwrap();
        assert_eq!(settings.client_secret().expose(), want, "{text:?}");
    }
}

#[test]
fn an_empty_or_unreadable_secret_file_is_refused_naming_only_the_path() {
    let dir = tempfile::tempdir().unwrap();
    for text in ["", "\n", "\r\n"] {
        let file = secret(dir.path(), text);
        let err = full("https://idp.example.com", &file, false).unwrap_err();
        assert!(
            matches!(err, ConfigError::OidcSecretFile { .. }),
            "{text:?}: {err:?}"
        );
        assert!(err.to_string().contains("client-secret"), "{err}");
    }
    let missing = dir.path().join("absent");
    let err = full("https://idp.example.com", &missing, false).unwrap_err();
    assert!(matches!(err, ConfigError::OidcSecretFile { .. }), "{err:?}");
    // A directory cannot be read as text; the error names the path and nothing it holds.
    let err = full("https://idp.example.com", dir.path(), false).unwrap_err();
    assert!(matches!(err, ConfigError::OidcSecretFile { .. }), "{err:?}");
}

#[test]
fn the_name_defaults_to_the_issuer_host_and_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    let named = |name: &str| {
        OidcSettings::from_flags(
            Some("https://sso.acme.example/realms/x"),
            Some("potluck"),
            Some(&file),
            Some(name),
            false,
            false,
        )
    };
    let default = full("https://sso.acme.example/realms/x", &file, false)
        .unwrap()
        .unwrap();
    assert_eq!(default.name(), "sso.acme.example");
    assert_eq!(named("  Acme SSO ").unwrap().unwrap().name(), "Acme SSO");
    assert_eq!(
        named(&"é".repeat(64)).unwrap().unwrap().name(),
        "é".repeat(64)
    );
    for bad in ["", "   ", &"a".repeat(65)] {
        assert!(
            matches!(named(bad).unwrap_err(), ConfigError::InvalidOidcName),
            "{bad:?}"
        );
    }
}

#[test]
fn jit_is_off_unless_asked() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    assert!(
        !full("https://idp.example.com", &file, false)
            .unwrap()
            .unwrap()
            .jit()
    );
    let on = OidcSettings::from_flags(
        Some("https://idp.example.com"),
        Some("potluck"),
        Some(&file),
        None,
        true,
        false,
    );
    assert!(on.unwrap().unwrap().jit());
}

#[test]
fn debug_never_shows_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret-value");
    let settings = full("https://idp.example.com", &file, false)
        .unwrap()
        .unwrap();
    let text = format!("{settings:?}");
    assert!(!text.contains("s3cret-value"), "{text}");
    assert!(text.contains("redacted"), "{text}");
}

#[test]
fn serve_refuses_a_partial_set_as_incomplete() {
    let err = parse_config(&[
        "--public-url",
        "https://p.example.com",
        "--oidc-client-id",
        "potluck",
    ])
    .unwrap_err();
    assert!(matches!(err, ConfigError::OidcIncomplete { .. }), "{err:?}");
}

#[test]
fn a_complete_set_reaches_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    let config = parse_config(&[
        "--public-url",
        "https://p.example.com",
        "--oidc-issuer",
        "https://idp.example.com",
        "--oidc-client-id",
        "potluck",
        "--oidc-client-secret-file",
        file.to_str().unwrap(),
        "--oidc-name",
        "Acme",
    ])
    .unwrap();
    let oidc = config.oidc().unwrap();
    assert_eq!(
        (oidc.issuer(), oidc.name()),
        ("https://idp.example.com", "Acme")
    );
    assert!(
        !format!("{config:?}").contains("s3cret"),
        "Config's Debug hides the secret"
    );
    assert!(
        parse_config(&["--public-url", "https://p.example.com"])
            .unwrap()
            .oidc()
            .is_none()
    );
}

#[test]
fn serve_passes_insecure_http_through_to_the_issuer_check() {
    let dir = tempfile::tempdir().unwrap();
    let file = secret(dir.path(), "s3cret");
    let base = [
        "--public-url",
        "https://p.example.com",
        "--oidc-issuer",
        "http://idp.example.com",
        "--oidc-client-id",
        "potluck",
        "--oidc-client-secret-file",
        file.to_str().unwrap(),
    ];
    let mut insecure = base.to_vec();
    insecure.push("--insecure-http");
    let config = parse_config(&insecure).unwrap();
    assert_eq!(config.oidc().unwrap().issuer(), "http://idp.example.com");
    let err = parse_config(&base).unwrap_err();
    assert!(
        matches!(err, ConfigError::InsecureOidcIssuer { .. }),
        "{err:?}"
    );
}
