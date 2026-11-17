//! The in-tree Apinata kind seen through the potluck-kind contract.

use potluck::kinds::all;
use potluck::kinds::apinata::{Apinata, detect_format};
use potluck_kind::{Blob, Change, Code, Finding, RULES_ABI, Role, RuleSet};

const BAD: &[u8] = include_bytes!("fixtures/secret-check/bad/env-secret-valued.yml");

#[test]
fn apinata_states_its_contract() {
    assert_eq!(all().len(), 1);
    let k = all()[0];
    assert_eq!(
        (k.kind(), k.abi(), k.formats(), k.format_file()),
        (
            "apinata",
            RULES_ABI,
            &["1.0.0"] as &[&str],
            "opencollection.yml"
        )
    );
}

#[test]
fn the_seed_detects_to_a_listed_format() {
    let k = all()[0];
    let file = k
        .seed()
        .into_iter()
        .find(|f| f.path() == k.format_file())
        .unwrap();
    let found = k.detect_format(file.bytes()).unwrap();
    assert!(k.formats().contains(&found.as_str()), "{found}");
}

#[test]
fn the_format_detector_reads_only_the_top_level_version() {
    for (bytes, want) in [
        (&b"opencollection: 1.0.0\n"[..], Some("1.0.0")),
        (b"opencollection: \"1.0.0\"\n", Some("1.0.0")),
        (b"opencollection: '1.0.0'\n", Some("1.0.0")),
        (
            b"\xef\xbb\xbfopencollection: 1.0.0\r\ninfo: {}\r\n",
            Some("1.0.0"),
        ),
        (
            b"info:\n  name: x\nopencollection: 2.0.0 # newer\n",
            Some("2.0.0"),
        ),
        (b"info:\n  opencollection: 1.0.0\n", None),
        (b"opencollection:\n", None),
        (b"name: x\n", None),
        (b"", None),
        (b"opencollection: \xff\n", None),
    ] {
        assert_eq!(
            detect_format(bytes).as_deref(),
            want,
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn check_ignores_baseline_and_role() {
    let p = "environments/dev.yml";
    let history = Apinata.check(&Change::history(p, Blob::Bytes(BAD), Role::Admin));
    let tip = Apinata.check(&Change::tip(p, None, Some(Blob::Bytes(BAD)), Role::Write));
    assert_eq!(history, tip);
    assert_eq!(
        history,
        vec![Finding::new(Code::SecretValue, "secret_value", p)]
    );
    assert!(
        Apinata
            .check(&Change::tip(p, Some(Blob::Bytes(BAD)), None, Role::Write))
            .is_empty()
    );
    assert!(
        history.iter().all(|f| f.detail().is_none()),
        "in-tree rules emit no detail, so no value can leak through one"
    );
}

#[test]
fn check_reads_only_environment_files_and_refuses_an_unread_one() {
    assert!(
        Apinata
            .check(&Change::history(
                "notes/x.yml",
                Blob::Bytes(BAD),
                Role::Write
            ))
            .is_empty()
    );
    assert_eq!(
        Apinata.check(&Change::history(
            "environments/big.yml",
            Blob::TooLarge,
            Role::Write
        )),
        vec![Finding::new(
            Code::SecretValue,
            "env_unparsable",
            "environments/big.yml"
        )]
    );
    assert!(Apinata.reads("environments/dev.yml") && !Apinata.reads("notes/dev.yml"));
}
