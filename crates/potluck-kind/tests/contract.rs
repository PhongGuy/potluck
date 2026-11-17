//! The shared contract: wire codes, value types, and a rule set usable as a trait object.

use potluck_kind::{Blob, Change, Code, Finding, RULES_ABI, Role, RuleSet, SeedFile};

#[test]
fn codes_are_the_wire_strings() {
    let all = [
        (Code::EnvFile, "secret_env_file"),
        (Code::SecretValue, "secret_value"),
        (Code::SecretPattern, "secret_pattern"),
        (Code::FormatUnsupported, "format_unsupported"),
        (Code::SafetyRelaxation, "safety_relaxation"),
    ];
    for (code, wire) in all {
        assert_eq!(code.as_str(), wire);
    }
    assert_eq!(RULES_ABI, 1);
}

#[test]
fn a_finding_carries_code_rule_path_and_an_optional_detail() {
    let f = Finding::new(Code::SecretValue, "secret_value", "environments/dev.yml");
    assert_eq!(
        (f.code(), f.rule(), f.path(), f.detail()),
        (
            Code::SecretValue,
            "secret_value",
            "environments/dev.yml",
            None
        )
    );
    let d = f.clone().with_detail("connections.prod.password");
    assert_eq!(d.detail(), Some("connections.prod.password"));
    assert_ne!(f, d, "the detail is part of the finding");
}

#[test]
fn history_has_no_baseline_and_tip_carries_both_sides() {
    let h = Change::history("a.yml", Blob::Bytes(b"x"), Role::Write);
    assert_eq!(
        (
            h.path(),
            h.has_baseline(),
            h.old_blob(),
            h.new_blob(),
            h.role()
        ),
        ("a.yml", false, None, Some(Blob::Bytes(b"x")), Role::Write)
    );
    let t = Change::tip("a.yml", Some(Blob::TooLarge), None, Role::Admin);
    assert_eq!(
        (t.has_baseline(), t.old_blob(), t.new_blob(), t.role()),
        (true, Some(Blob::TooLarge), None, Role::Admin)
    );
}

#[test]
fn a_seed_file_is_static_path_and_bytes() {
    let s = SeedFile::new(".gitignore", b".env\n");
    assert_eq!((s.path(), s.bytes()), (".gitignore", b".env\n" as &[u8]));
}

#[derive(Debug)]
struct Minimal;

impl RuleSet for Minimal {
    fn kind(&self) -> &'static str {
        "minimal"
    }
    fn abi(&self) -> u32 {
        RULES_ABI
    }
    fn formats(&self) -> &'static [&'static str] {
        &["1"]
    }
    fn format_file(&self) -> &'static str {
        "minimal.txt"
    }
    fn detect_format(&self, _format_file: &[u8]) -> Option<String> {
        Some("1".to_owned())
    }
    fn path_rule(&self, _path: &str) -> Option<Finding> {
        None
    }
    fn reads(&self, _path: &str) -> bool {
        false
    }
    fn check(&self, _change: &Change<'_>) -> Vec<Finding> {
        Vec::new()
    }
    fn seed(&self) -> Vec<SeedFile> {
        Vec::new()
    }
}

#[test]
fn a_rule_set_is_usable_as_a_static_trait_object() {
    static SET: Minimal = Minimal;
    let dynamic: &'static dyn RuleSet = &SET;
    assert_eq!(
        (dynamic.kind(), dynamic.formats()),
        ("minimal", &["1"] as &[&str])
    );
}

#[test]
fn the_crate_has_no_dependencies_of_its_own() {
    let manifest = include_str!("../Cargo.toml");
    for section in [
        "[dependencies]",
        "[dev-dependencies]",
        "[build-dependencies]",
    ] {
        assert!(!manifest.contains(section), "{section} in {manifest}");
    }
}
