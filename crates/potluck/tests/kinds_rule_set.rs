//! Every registered rule set seen through the potluck-kind contract, and the Apinata rule set
//! Potluck pins from `apinata-rules`.

use potluck::kinds::{all, by_name};
use potluck_kind::{Blob, Change, Code, Finding, RULES_ABI, Role, RuleSet};

const BAD: &[u8] = include_bytes!("fixtures/secret-check/bad/env-secret-valued.yml");
const ENV: &str = "environments/dev.yml";

fn apinata() -> &'static dyn RuleSet {
    by_name("apinata").expect("apinata is registered")
}

#[test]
fn every_rule_set_targets_this_abi() {
    assert!(!all().is_empty(), "no rule set registered");
    for k in all() {
        assert_eq!(k.abi(), RULES_ABI, "{}", k.kind());
    }
}

#[test]
fn apinata_is_the_only_kind_and_states_its_contract() {
    let kinds: Vec<&str> = all().iter().map(|k| k.kind()).collect();
    assert_eq!(kinds, ["apinata"]);
    assert_eq!(
        (apinata().formats(), apinata().format_file()),
        (&["1.0.0"] as &[&str], "opencollection.yml")
    );
}

#[test]
fn by_name_finds_every_rule_set_under_its_own_name() {
    for k in all() {
        assert_eq!(by_name(k.kind()).map(|f| f.kind()), Some(k.kind()));
    }
    assert!(by_name("no-such-kind").is_none());
    assert!(
        by_name("dbento").is_none(),
        "dbento has no rule set before phase 2"
    );
}

#[test]
fn every_seed_detects_to_a_listed_format() {
    for k in all() {
        let seed = k.seed();
        let file = seed
            .iter()
            .find(|f| f.path() == k.format_file())
            .unwrap_or_else(|| panic!("{} seeds no {}", k.kind(), k.format_file()));
        let found = k.detect_format(file.bytes());
        assert!(
            found.as_deref().is_some_and(|v| k.formats().contains(&v)),
            "{}: {found:?}",
            k.kind()
        );
    }
}

#[test]
fn the_apinata_seed_is_the_one_potluck_shipped() {
    // The seed stays byte-identical, so SEED_COMMIT and existing workspaces hold.
    let seed = apinata().seed();
    let files: Vec<(&str, &[u8])> = seed.iter().map(|f| (f.path(), f.bytes())).collect();
    assert_eq!(
        files,
        [
            (".gitignore", &b".env\n"[..]),
            (
                "opencollection.yml",
                &b"opencollection: 1.0.0\n\ninfo:\n  name: Workspace\nbundled: false\nextensions:\n  bruno:\n    ignore:\n      - node_modules\n      - .git\n"[..]
            ),
        ]
    );
}

#[test]
fn check_ignores_baseline_and_role() {
    // The tip pass relies on this: a tip finding equals the history finding for the same blob.
    let k = apinata();
    let history = k.check(&Change::history(ENV, Blob::Bytes(BAD), Role::Admin));
    let tip = k.check(&Change::tip(ENV, None, Some(Blob::Bytes(BAD)), Role::Write));
    let over_old = k.check(&Change::tip(
        ENV,
        Some(Blob::Bytes(b"name: dev\n")),
        Some(Blob::Bytes(BAD)),
        Role::Read,
    ));
    assert_eq!(history, tip);
    assert_eq!(history, over_old);
    assert_eq!(
        history,
        vec![Finding::new(Code::SecretValue, "secret_value", ENV).with_detail("api_key")]
    );
    assert!(
        k.check(&Change::tip(ENV, Some(Blob::Bytes(BAD)), None, Role::Write))
            .is_empty()
    );
}

#[test]
fn check_reads_only_environment_files_and_refuses_an_unread_one() {
    let k = apinata();
    assert!(
        k.check(&Change::history(
            "notes/x.yml",
            Blob::Bytes(BAD),
            Role::Write
        ))
        .is_empty()
    );
    assert_eq!(
        k.check(&Change::history(
            "environments/big.yml",
            Blob::TooLarge,
            Role::Write
        )),
        vec![
            Finding::new(Code::SecretValue, "env_unparsable", "environments/big.yml")
                .with_detail("too large to check")
        ]
    );
    assert!(k.reads("environments/dev.yml"));
    assert!(!k.reads("notes/dev.yml"));
    assert!(!k.reads("environments/sub/dev.yml"));
}
