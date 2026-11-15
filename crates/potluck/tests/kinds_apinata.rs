//! The apinata kind: environment files, the YAML walk and the seed.

use potluck::kinds::apinata::{
    Apinata, ENV_UNPARSABLE, NODE_CAP, SECRET_VALUE, environment_rule, is_environment_file, seed,
};
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
    Some(Finding::new(Code::SecretValue, SECRET_VALUE))
}

fn unparsable() -> Option<Finding> {
    Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE))
}

#[test]
fn rule_names_are_the_contract_names() {
    assert_eq!(SECRET_VALUE, "secret_value");
    assert_eq!(ENV_UNPARSABLE, "env_unparsable");
    assert_eq!(unparsable().map(|f| f.code()), Some(Code::SecretValue));
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
fn unvalued_false_and_null_pass() {
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
    ] {
        assert_eq!(
            Apinata.content_rule("environments/x.yml", bytes),
            None,
            "{name}"
        );
    }
}

#[test]
fn an_empty_file_is_one_null_document_and_passes() {
    assert_eq!(Apinata.content_rule("environments/x.yml", b""), None);
    assert_eq!(Apinata.content_rule("environments/x.yml", b"\n  \n"), None);
}

#[test]
fn files_apinata_cannot_read_are_refused() {
    for (name, bytes) in [
        (
            "env-two-documents.yml",
            fixture!("bad", "env-two-documents.yml"),
        ),
        ("env-not-yaml.yml", fixture!("bad", "env-not-yaml.yml")),
        (
            "env-duplicate-key-top.yml",
            fixture!("bad", "env-duplicate-key-top.yml"),
        ),
        (
            "env-duplicate-key-variable.yml",
            fixture!("bad", "env-duplicate-key-variable.yml"),
        ),
        (
            "env-deep-nesting.yml",
            fixture!("bad", "env-deep-nesting.yml"),
        ),
        ("env-alias-bomb.yml", fixture!("bad", "env-alias-bomb.yml")),
        ("unterminated flow sequence", b"variables: [1, 2" as &[u8]),
        ("invalid utf-8", b"name: \xff\xfe\n" as &[u8]),
        ("unknown alias", b"name: *nowhere\n" as &[u8]),
    ] {
        assert_eq!(
            Apinata.content_rule("environments/x.yml", bytes),
            unparsable(),
            "{name}"
        );
    }
}

#[test]
fn unparsable_is_only_for_environment_files() {
    let bytes = fixture!("bad", "env-not-yaml.yml");
    assert_eq!(Apinata.content_rule("notes/x.yml", bytes), None);
    assert_eq!(Apinata.content_rule("environments/x.yaml", bytes), None);
}

#[test]
fn the_seed_collection_passes_the_kind_rule() {
    let files = Apinata.seed();
    assert_eq!(
        Apinata.content_rule("opencollection.yml", files[1].bytes()),
        None
    );
}

#[test]
fn a_tagged_mapping_cannot_hide_a_valued_secret() {
    let bytes = b"name: dev\nv: !custom {secret: true, value: fixture-tagged}\n";
    assert_eq!(
        Apinata.content_rule("environments/x.yml", bytes),
        hit(),
        "the walk descends through a custom tag"
    );
}

#[test]
fn every_seed_file_passes_the_yaml_walk() {
    for file in Apinata.seed() {
        assert_eq!(
            Apinata.content_rule("environments/x.yml", file.bytes()),
            None,
            "{} under an environment path",
            file.path()
        );
    }
}

#[test]
fn the_node_cap_is_two_million() {
    assert_eq!(NODE_CAP, 2_000_000);
}

#[test]
fn nodes_are_counted_after_alias_expansion_keys_included() {
    // sequence + 3 scalars
    assert_eq!(environment_rule(b"[1, 2, 3]", 4), None);
    assert_eq!(environment_rule(b"[1, 2, 3]", 3), unparsable());
    // root map, key a, sequence of 3 (4), key b, the alias copy (4)
    let aliased = b"a: &a [1, 2, 3]\nb: *a\n";
    assert_eq!(environment_rule(aliased, 11), None);
    assert_eq!(environment_rule(aliased, 10), unparsable());
    // a tag wrapper is free: root map, key v, scalar
    assert_eq!(environment_rule(b"v: !t 1\n", 3), None);
    assert_eq!(environment_rule(b"v: !t 1\n", 2), unparsable());
    // an empty document is one null node
    assert_eq!(environment_rule(b"", 1), None);
    assert_eq!(environment_rule(b"", 0), unparsable());
}

#[test]
fn a_valued_secret_under_the_cap_is_still_secret_value() {
    let bad = fixture!("bad", "env-secret-valued.yml");
    assert_eq!(environment_rule(bad, NODE_CAP), hit());
    assert_eq!(
        environment_rule(bad, 5),
        unparsable(),
        "the cap is checked first"
    );
}

fn wide(aliases: usize) -> Vec<u8> {
    let mut s = String::from("a: &a [");
    s.push_str(&vec!["x"; 4000].join(", "));
    s.push_str("]\nb: [");
    s.push_str(&vec!["*a"; aliases].join(", "));
    s.push_str("]\n");
    s.into_bytes()
}

// The one test at the real cap: 0.67 s in a release build, slower in the unoptimised test
// profile. Every other cap test uses a small `node_cap` through `environment_rule`.
#[test]
fn the_real_cap_refuses_a_wide_alias_fan_out() {
    // 1 + 1 + 4001 + 1 + 1 + 501 * 4001 = 2_008_506 nodes, 501 jumps: yaml_serde accepts it.
    assert_eq!(
        Apinata.content_rule("environments/x.yml", &wide(501)),
        unparsable()
    );
    // The accepted side of the real cap is deliberately untested: the small-cap boundary
    // tests cover it, and a 1,996,503-node document costs as much as the refused one.
}

#[test]
fn the_counted_parse_keeps_every_refusal_of_the_plain_parse() {
    for (name, bytes) in [
        (
            "env-duplicate-key-variable.yml",
            fixture!("bad", "env-duplicate-key-variable.yml"),
        ),
        (
            "env-deep-nesting.yml",
            fixture!("bad", "env-deep-nesting.yml"),
        ),
        ("env-alias-bomb.yml", fixture!("bad", "env-alias-bomb.yml")),
        ("flow duplicate", b"{a: 1, a: 2}" as &[u8]),
    ] {
        assert_eq!(environment_rule(bytes, NODE_CAP), unparsable(), "{name}");
    }
    assert_eq!(
        environment_rule(
            b"name: dev\nv: !custom {secret: true, value: fixture-tagged}\n",
            NODE_CAP
        ),
        hit(),
        "a tagged mapping is still walked"
    );
}
