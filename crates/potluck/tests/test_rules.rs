//! Rule sets that misbehave or report the new codes, for the scan and hook tests.

use potluck::kinds::{Blob, Change, Code, Finding, RULES_ABI, RuleSet, SeedFile};

/// What a `TestRules` does when `check` sees a file whose path ends in `.t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Panic,
    PanicInPathRule,
    PanicInReads,
    BadPathRuleName,
    Plain,
    BadRuleName,
    BadLaterFinding,
    EmptyRuleName,
    Detail(&'static str),
    Elsewhere,
    Relax,
    RelaxAlways,
    TwoRules,
    SameRuleTwice,
    PlainAndRelax,
    TwoRelaxations,
}

#[derive(Debug)]
pub struct TestRules {
    pub act: Act,
    pub abi: u32,
}

pub const fn rules(act: Act) -> TestRules {
    TestRules {
        act,
        abi: RULES_ABI,
    }
}

impl RuleSet for TestRules {
    fn kind(&self) -> &'static str {
        "test"
    }
    fn abi(&self) -> u32 {
        self.abi
    }
    fn formats(&self) -> &'static [&'static str] {
        &["1"]
    }
    fn format_file(&self) -> &'static str {
        "format.t"
    }
    fn detect_format(&self, bytes: &[u8]) -> Option<String> {
        std::str::from_utf8(bytes).ok().map(|s| s.trim().to_owned())
    }
    fn path_rule(&self, path: &str) -> Option<Finding> {
        assert!(
            !(self.act == Act::PanicInPathRule && path.ends_with(".t")),
            "PANIC-SECRET-TEXT in path_rule"
        );
        (self.act == Act::BadPathRuleName && path.ends_with(".t"))
            .then(|| Finding::new(Code::SecretValue, "Bad Path", path))
    }
    fn reads(&self, path: &str) -> bool {
        assert!(
            !(self.act == Act::PanicInReads && path.ends_with(".t")),
            "PANIC-SECRET-TEXT in reads"
        );
        path.ends_with(".t")
    }
    fn check(&self, change: &Change<'_>) -> Vec<Finding> {
        let p = change.path();
        match self.act {
            Act::Panic => panic!("PANIC-SECRET-TEXT in check"),
            Act::PanicInPathRule | Act::PanicInReads | Act::BadPathRuleName => Vec::new(),
            Act::Plain => vec![Finding::new(Code::SecretValue, "plain", p)],
            Act::BadRuleName => vec![Finding::new(Code::SecretValue, "Bad Rule", p)],
            Act::Detail(d) => {
                vec![Finding::new(Code::SecretValue, "with_detail", p).with_detail(d)]
            }
            Act::BadLaterFinding => vec![
                Finding::new(Code::SecretValue, "plain", p),
                Finding::new(Code::SecretValue, "Bad Later", p),
            ],
            Act::EmptyRuleName => vec![Finding::new(Code::SecretValue, "", p)],
            Act::Elsewhere => vec![Finding::new(
                Code::SecretValue,
                "elsewhere",
                "other/place.t",
            )],
            // Reported for every role; Potluck decides. Only a tip change whose new side says
            // "relaxed" while the old side did not is a relaxation.
            Act::Relax if relaxing(change) => {
                vec![Finding::new(Code::SafetyRelaxation, "relax_test", p)]
            }
            Act::Relax => Vec::new(),
            // Reported at every path the kind reads, history and tip alike, whatever the role.
            Act::RelaxAlways => vec![Finding::new(Code::SafetyRelaxation, "relax_always", p)],
            Act::TwoRules => vec![
                Finding::new(Code::SecretValue, "first", p),
                Finding::new(Code::SecretValue, "second", p),
            ],
            // The same rule at the same path twice, told apart only by detail.
            Act::SameRuleTwice => vec![
                Finding::new(Code::SecretValue, "twice", p).with_detail("one"),
                Finding::new(Code::SecretValue, "twice", p).with_detail("two"),
            ],
            // A plain finding on every change, then a relaxation on a relaxing tip change.
            Act::PlainAndRelax => {
                let mut found = vec![Finding::new(Code::SecretValue, "plain", p)];
                if relaxing(change) {
                    found.push(Finding::new(Code::SafetyRelaxation, "relax_test", p));
                }
                found
            }
            Act::TwoRelaxations if relaxing(change) => vec![
                Finding::new(Code::SafetyRelaxation, "relax_one", p),
                Finding::new(Code::SafetyRelaxation, "relax_two", p),
            ],
            Act::TwoRelaxations => Vec::new(),
        }
    }
    fn seed(&self) -> Vec<SeedFile> {
        Vec::new()
    }
}

/// A tip change whose new side starts with "relaxed" while its old side does not.
fn relaxing(change: &Change<'_>) -> bool {
    let says = |b: Option<Blob<'_>>| matches!(b, Some(Blob::Bytes(x)) if x.starts_with(b"relaxed"));
    change.has_baseline() && says(change.new_blob()) && !says(change.old_blob())
}
