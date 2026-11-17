//! scripts/check-public-api.sh over hand-made rustdoc JSON: a leak fails, blanket impls and doc
//! links do not count, and JSON it cannot read is refused rather than passed.

use std::process::{Command, Output};

use serde_json::Value;

use crate::common::repo_root;

/// A public `router` returning `axum::routing::Router`, a function returning `String`, a
/// blanket `FromRef` impl from axum on `AppState`, and a doc link to `http::HeaderMap`.
const LEAKY: &str = r#"{"format_version": 61,
 "external_crates": {"5": {"name": "axum"}, "6": {"name": "alloc"}, "7": {"name": "http"}},
 "paths": {
   "10": {"crate_id": 5, "path": ["axum", "routing", "Router"], "kind": "struct"},
   "11": {"crate_id": 6, "path": ["alloc", "string", "String"], "kind": "struct"},
   "12": {"crate_id": 7, "path": ["http", "header", "map", "HeaderMap"], "kind": "struct"},
   "2": {"crate_id": 0, "path": ["potluck", "api", "AppState"], "kind": "struct"}},
 "index": {
   "1": {"id": 1, "crate_id": 0, "name": "router", "span": {"filename": "src/api/mod.rs", "begin": [160, 0]},
         "inner": {"function": {"sig": {"output": {"resolved_path": {"path": "Router", "id": 10, "args": null}}}}}},
   "2": {"id": 2, "crate_id": 0, "name": "AppState", "inner": {"struct": {}}},
   "3": {"id": 3, "crate_id": 0, "name": "name", "inner": {"function": {"sig": {"output": {"resolved_path": {"path": "String", "id": 11}}}}}},
   "4": {"id": 4, "crate_id": 0, "name": null, "inner": {"impl": {"blanket_impl": {"generic": "T"}, "is_synthetic": false,
         "trait": {"path": "FromRef", "id": 10}, "for": {"resolved_path": {"path": "AppState", "id": 2}}}}},
   "5": {"id": 5, "crate_id": 0, "name": "documented", "docs": "Not a [`HeaderMap`].", "links": {"`HeaderMap`": 12},
         "inner": {"function": {"sig": {"output": null}}}}
 }}"#;

fn check(json: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("potluck.json");
    std::fs::write(&path, json).unwrap();
    Command::new("bash")
        .arg(repo_root().join("scripts/check-public-api.sh"))
        .arg(&path)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_public_item_naming_an_axum_type_fails_the_check() {
    let out = check(LEAKY);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert_eq!(
        text,
        "check-public-api: public items name a web-stack type:\n\
         router (src/api/mod.rs:160): axum::routing::Router\n",
        "only the signature leak is listed: not String, not the blanket impl, not the doc link"
    );
}

#[test]
fn without_the_leak_the_check_passes() {
    let mut json: Value = serde_json::from_str(LEAKY).unwrap();
    json["index"].as_object_mut().unwrap().remove("1");
    let out = check(&json.to_string());
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(
        stdout(&out),
        "check-public-api: no axum, tower, http or hyper type in the public API\n"
    );
}

#[test]
fn json_it_cannot_read_is_refused() {
    for json in [
        "{}",
        r#"{"format_version": 61, "index": {}, "paths": {}, "external_crates": {}}"#,
    ] {
        let out = check(json);
        assert_eq!(out.status.code(), Some(2), "{json}: {}", stdout(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.starts_with("check-public-api: ")
                && stderr.ends_with("/potluck.json is not rustdoc JSON this check can read\n")
                && stderr.lines().count() == 1,
            "{json}: {stderr}"
        );
    }
}
