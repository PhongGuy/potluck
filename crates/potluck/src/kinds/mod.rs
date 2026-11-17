//! Workspace kinds: each kind's push rules and seed files. Nothing outside this module
//! knows an app's file format.

pub mod apinata;
pub mod generic;

pub use potluck_kind::{Blob, Change, Code, Finding, RULES_ABI, RuleSet, SeedFile};

/// The rule set for `name`: `apinata` now; `dbento` is reserved and answers `None`.
pub fn by_name(name: &str) -> Option<&'static dyn RuleSet> {
    all().iter().copied().find(|k| k.kind() == name)
}

/// Every registered rule set, as the potluck-kind contract sees it.
pub fn all() -> &'static [&'static dyn RuleSet] {
    static ALL: [&dyn RuleSet; 1] = [apinata_rules::RULE_SET];
    &ALL
}
