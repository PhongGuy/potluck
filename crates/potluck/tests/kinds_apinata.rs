//! The apinata kind: environment files, the YAML walk and the seed.

use potluck::kinds::apinata::{
    Apinata, ENV_UNPARSABLE, NODE_CAP, SECRET_VALUE, environment_rule, is_environment_file, seed,
};
use potluck::kinds::generic;
use potluck::kinds::{Code, Finding, Kind, by_name};

const SEED_COLLECTION: &[u8] = b"opencollection: 1.0.0\n\ninfo:\n  name: Workspace\nbundled: false\nextensions:\n  bruno:\n    ignore:\n      - node_modules\n      - .git\n";

#[test]
fn only_root_environment_yml_files_are_checked() {
    for p in [
        "environments/dev.yml",
        "environments/Dev.yml",
        "environments/a-b_c.yml",
    ] {
        assert!(is_environment_file(p), "{p}");
    }
    for p in [
        "Environments/dev.yml",
        "sub/environments/dev.yml",
        "collections/api/environments/dev.yml",
        "environments/dev.yaml",
        "environments/dev.YML",
        "environments/nested/dev.yml",
        "my-environments/dev.yml",
        "dev.yml",
        "environments.yml",
        "environments/dev.yml.bak",
        "/environments/dev.yml",
        "-",
        "/",
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
    assert_eq!(
        Apinata.content_rule("api/environments/dev.yml", bad),
        None,
        "Apinata reads only the root directory"
    );
    assert_eq!(Apinata.content_rule("Environments/dev.yml", bad), None);
    assert_eq!(Apinata.content_rule("environments/dev.yaml", bad), None);
    assert_eq!(Apinata.content_rule("notes/dev.yml", bad), None);
    assert_eq!(Apinata.content_rule("-", bad), None);
    assert_eq!(Apinata.content_rule("/", bad), None);
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

// The one test at the real cap; every other cap test uses a small `node_cap` through
// `environment_rule`.
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

#[test]
fn an_alias_or_a_merge_cannot_hide_a_valued_secret() {
    for (name, bytes) in [
        (
            "env-alias-secret-value.yml",
            fixture!("bad", "env-alias-secret-value.yml"),
        ),
        (
            "env-merge-secret-value.yml",
            fixture!("bad", "env-merge-secret-value.yml"),
        ),
        (
            "aliased whole variable",
            b"name: dev\nvariables:\n  - &v\n    secret: true\n    name: a\n    value: x\n  - *v\n"
                as &[u8],
        ),
        (
            "merge list, value from the second mapping",
            b"a: &a {secret: true}\nb: &b {value: x}\nvariables:\n  - <<: [*a, *b]\n    name: k\n"
                as &[u8],
        ),
        (
            "merge brings secret, explicit value",
            b"a: &a {secret: true}\nvariables:\n  - <<: *a\n    name: k\n    value: x\n" as &[u8],
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
fn a_merge_without_a_value_passes() {
    assert_eq!(
        Apinata.content_rule(
            "environments/x.yml",
            fixture!("good", "env-merge-secret-unvalued.yml")
        ),
        None
    );
}

#[test]
fn an_explicit_null_value_beats_a_merged_value() {
    // The anchor alone carries no `secret`, the variable alone no value.
    let bytes =
        b"a: &a {value: x}\nvariables:\n  - <<: *a\n    secret: true\n    name: k\n    value:\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", bytes), None);
}

#[test]
fn a_merge_under_a_tag_is_still_applied() {
    let bytes = b"a: &a {value: x}\nv: !t {<<: *a, secret: true}\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", bytes), hit());
}

#[test]
fn a_merge_yaml_serde_cannot_apply_is_unparsable() {
    for (name, bytes) in [
        (
            "scalar merge",
            b"variables:\n  - <<: 1\n    name: k\n" as &[u8],
        ),
        ("tagged merge", b"v: {<<: !t {k: 1}}\n" as &[u8]),
        (
            "scalar in merge list",
            b"a: &a {secret: true}\nv: {<<: [*a, 1]}\n" as &[u8],
        ),
    ] {
        assert_eq!(
            Apinata.content_rule("environments/x.yml", bytes),
            unparsable(),
            "{name}"
        );
    }
}

#[test]
fn a_plain_merge_key_is_not_a_variable_key() {
    // The merge key vanishes; a mapping with only `<<` and `name` has no value.
    let bytes = b"a: &a {name: base}\nvariables:\n  - <<: *a\n    secret: true\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", bytes), None);
}

#[test]
fn an_alias_used_as_a_mapping_key_is_counted_and_read_as_its_value() {
    // root map, key a, sequence of 3 (4), the alias copy as a key (4), value v
    let counted = b"a: &a [1, 2, 3]\n? *a\n: v\n";
    assert_eq!(environment_rule(counted, 11), None);
    assert_eq!(environment_rule(counted, 10), unparsable());
    // An anchored `secret` scalar spelled as an alias key is still the `secret` key.
    let valued = b"k: &k secret\nvariables:\n  - *k : true\n    value: x\n    name: n\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", valued), hit());
    let unvalued = b"k: &k secret\nvariables:\n  - *k : true\n    name: n\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", unvalued), None);
}

#[test]
fn a_chained_merge_cannot_hide_a_valued_secret() {
    for (name, bytes) in [
        (
            "plain chain",
            b"base: &b {value: x}\nmid: &m {<<: *b}\nvariables:\n  - <<: *m\n    secret: true\n    name: k\n" as &[u8],
        ),
        (
            "chain through merge lists",
            b"base: &b {value: x}\nmid: &m {<<: [*b]}\nvariables:\n  - <<: [*m]\n    secret: true\n    name: k\n" as &[u8],
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
fn content_path_is_the_environment_file_glob() {
    assert!(Apinata.content_path("environments/dev.yml"));
    assert!(!Apinata.content_path("notes/dev.yml"));
    assert!(!Apinata.content_path("sub/environments/dev.yml"));
    assert!(!Apinata.content_path("/"));
}

#[test]
fn over_cap_rule_refuses_an_environment_file_the_hook_cannot_read() {
    assert_eq!(
        Apinata.over_cap_rule("environments/dev.yml"),
        Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE))
    );
    assert_eq!(Apinata.over_cap_rule("notes/big.bin"), None);
    assert_eq!(Apinata.over_cap_rule("environments/nested/dev.yml"), None);
}

#[test]
fn a_valued_secret_in_a_mapping_key_is_refused() {
    for (name, bytes) in [
        (
            "block complex key",
            b"variables:\n  - ? {secret: true, value: x}\n    : 1\n" as &[u8],
        ),
        (
            "flow complex key",
            b"v: {? {secret: true, value: x} : 1}\n" as &[u8],
        ),
        (
            "key of a key",
            b"? {? {secret: true, value: x} : 1}\n: 1\n" as &[u8],
        ),
        (
            "inside a sequence key",
            b"? [{secret: true, value: x}]\n: 1\n" as &[u8],
        ),
        (
            "tagged key",
            b"? !t {secret: true, value: x}\n: 1\n" as &[u8],
        ),
        (
            "inside a value of a key",
            b"? {v: {secret: true, value: x}}\n: 1\n" as &[u8],
        ),
        (
            "anchored in a key, aliased as a key",
            b"? &s {secret: true, value: x}\n: 1\nb: {? *s : 2}\n" as &[u8],
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
fn an_unvalued_secret_in_a_mapping_key_passes() {
    let bytes = b"? {secret: true, value: null}\n: 1\n";
    assert_eq!(Apinata.content_rule("environments/x.yml", bytes), None);
}

#[test]
fn a_secret_key_is_found_within_the_same_node_cap() {
    // root map, key map, 4 scalars, value 1
    let bytes = b"? {secret: true, value: x}\n: 1\n";
    assert_eq!(environment_rule(bytes, 7), hit());
    assert_eq!(environment_rule(bytes, 6), unparsable());
}

// Nested complex keys, the secret innermost: `{? {? ... {secret: true, value: x} : 1} : 1}`.
fn nested_keys(depth: usize) -> Vec<u8> {
    let mut s = "{? ".repeat(depth);
    s.push_str("{secret: true, value: x}");
    s.push_str(&" : 1}".repeat(depth));
    s.into_bytes()
}

#[test]
fn key_nesting_is_walked_to_the_parser_depth_limit_and_refused_past_it() {
    assert_eq!(environment_rule(&nested_keys(120), NODE_CAP), hit());
    assert_eq!(environment_rule(&nested_keys(200), NODE_CAP), unparsable());
}

#[test]
fn an_alias_bomb_expanding_only_into_keys_is_unparsable() {
    // Ten levels of ten-fold aliases, every anchor inside a key: 10^10 nodes unexpanded.
    let mut s = String::from("? &l0 [x, x, x, x, x, x, x, x, x, x]\n: 0\n");
    for i in 1..10 {
        let prev = format!("*l{}", i - 1);
        s.push_str(&format!("? &l{i} [{}]\n: {i}\n", vec![prev; 10].join(", ")));
    }
    assert_eq!(environment_rule(s.as_bytes(), NODE_CAP), unparsable());
}

// One 4000-scalar anchor, then `entries` distinct keys `[*a, i]` each expanding it, then a
// valued secret as the last key. 4003 + entries * 4004 + 6 nodes.
fn key_fan_out(entries: usize) -> Vec<u8> {
    let mut s = String::from("? &a [");
    s.push_str(&vec!["x"; 4000].join(", "));
    s.push_str("]\n: 0\n");
    for i in 0..entries {
        s.push_str(&format!("? [*a, {i}]\n: {i}\n"));
    }
    s.push_str("? {secret: true, value: x}\n: end\n");
    s.into_bytes()
}

// The walk over keys is one pass over the counted tree, so the real cap bounds it.
#[test]
fn a_key_alias_fan_out_is_walked_to_its_last_key_under_the_real_cap() {
    // 1,993,997 nodes: accepted by the cap, the secret in the last key found.
    assert_eq!(environment_rule(&key_fan_out(497), NODE_CAP), hit());
    // 2,010,013 nodes: refused by the cap before any walk.
    assert_eq!(environment_rule(&key_fan_out(501), NODE_CAP), unparsable());
}
