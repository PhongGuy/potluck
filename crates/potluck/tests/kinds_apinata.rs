//! The apinata kind: environment files, the YAML walk and the seed.

use potluck::kinds::apinata::{Apinata, is_environment_file, seed};
use potluck::kinds::generic;
use potluck::kinds::{Code, Finding, Kind, by_name};

const SEED_COLLECTION: &[u8] = b"opencollection: 1.0.0\n\ninfo:\n  name: Workspace\nbundled: false\nextensions:\n  bruno:\n    ignore:\n      - node_modules\n      - .git\n";

#[test]
fn environment_files_at_any_depth_are_checked_and_nothing_else_is() {
    for p in [
        "environments/dev.yml",
        "collections/api/environments/dev.yml",
    ] {
        assert!(is_environment_file(p), "{p}");
    }
    for p in [
        "environments/nested/dev.yml",
        "environments/dev.yaml",
        "my-environments/dev.yml",
        "dev.yml",
        "environments.yml",
        "environments/dev.yml.bak",
        "-",
        "",
    ] {
        assert!(!is_environment_file(p), "{p}");
    }
}

#[test]
fn the_seed_is_the_gitignore_and_the_vendored_collection_byte_for_byte() {
    let files = seed();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path(), ".gitignore");
    assert_eq!(files[0].bytes(), b".env\n");
    assert_eq!(files[0].bytes().len(), 5);
    assert_eq!(files[1].path(), "opencollection.yml");
    assert_eq!(
        files[1].bytes(),
        SEED_COLLECTION,
        "the contract Apinata vendors"
    );
    assert_eq!(files[1].bytes().len(), 129);
    assert_eq!(
        generic::content_rule(files[1].bytes()),
        None,
        "the seed passes its own server's check"
    );
    assert_eq!(generic::path_rule(files[0].path()), None);
}

macro_rules! fixture {
    ($dir:literal, $name:literal) => {
        include_bytes!(concat!("fixtures/secret-check/", $dir, "/", $name)) as &[u8]
    };
}

fn hit() -> Option<Finding> {
    Some(Finding::new(Code::SecretValue, "secret_value"))
}

#[test]
fn by_name_knows_apinata_only() {
    assert_eq!(by_name("apinata").map(Kind::name), Some("apinata"));
    assert!(
        by_name("dbento").is_none(),
        "reserved, implemented under issue #3"
    );
    assert!(by_name("Apinata").is_none(), "names are exact");
    assert!(by_name("").is_none());
    assert_eq!(by_name("apinata").unwrap().seed(), seed());
}

#[test]
fn the_rule_follows_the_path() {
    let bad = fixture!("bad", "env-secret-valued.yml");
    assert_eq!(Apinata.content_rule("environments/dev.yml", bad), hit());
    assert_eq!(Apinata.content_rule("api/environments/dev.yml", bad), hit());
    assert_eq!(Apinata.content_rule("notes/dev.yml", bad), None);
    assert_eq!(Apinata.content_rule("-", bad), None);
    assert_eq!(Apinata.path_rule("environments/dev.yml"), None);
    assert_eq!(Apinata.path_rule(".env"), None, "the generic set owns .env");
}

#[test]
fn valued_secrets_are_refused_in_every_shape() {
    for (name, bytes) in [
        (
            "env-secret-valued.yml",
            fixture!("bad", "env-secret-valued.yml"),
        ),
        (
            "env-secret-typed-value.yml",
            fixture!("bad", "env-secret-typed-value.yml"),
        ),
        (
            "env-secret-nested.yml",
            fixture!("bad", "env-secret-nested.yml"),
        ),
    ] {
        assert_eq!(
            Apinata.content_rule("environments/x.yml", bytes),
            hit(),
            "{name}"
        );
    }
}

#[test]
fn an_empty_string_value_is_still_a_value() {
    let bytes = b"variables:\n  - secret: true\n    name: k\n    value: ''\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", bytes), hit());
}

#[test]
fn unvalued_false_null_and_unparsable_pass() {
    for (name, bytes) in [
        (
            "env-secret-unvalued.yml",
            fixture!("good", "env-secret-unvalued.yml"),
        ),
        (
            "env-secret-false.yml",
            fixture!("good", "env-secret-false.yml"),
        ),
        (
            "env-secret-null-value.yml",
            fixture!("good", "env-secret-null-value.yml"),
        ),
        ("env-not-yaml.yml", fixture!("good", "env-not-yaml.yml")),
    ] {
        assert_eq!(
            Apinata.content_rule("environments/x.yml", bytes),
            None,
            "{name}"
        );
    }
    assert_eq!(Apinata.content_rule("environments/x.yml", b""), None);
    assert_eq!(
        Apinata.content_rule("environments/x.yml", &[0xff, 0xfe, b':']),
        None,
        "invalid UTF-8 is not an environment file"
    );
}

#[test]
fn a_multi_document_file_passes_unscanned() {
    // Pinned, not desired: yaml_serde reads one document, so a second one is invisible.
    assert_eq!(
        Apinata.content_rule(
            "environments/x.yml",
            fixture!("good", "env-two-documents.yml")
        ),
        None
    );
}

#[test]
fn the_seed_collection_passes_the_kind_rule() {
    let files = Apinata.seed();
    assert_eq!(
        Apinata.content_rule("opencollection.yml", files[1].bytes()),
        None
    );
}
