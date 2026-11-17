//! Rule sets that misbehave or report the new codes, for the scan and hook tests.

use potluck::kinds::{Change, Code, Finding, RULES_ABI, RuleSet, SeedFile};

/// What a `TestRules` does when `check` sees a file whose path ends in `.t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Panic,
    PanicInPathRule,
    PanicInReads,
    BadPathRuleName,
    Plain,
    BadRuleName,
    Detail(&'static str),
    Elsewhere,
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
            Act::Elsewhere => vec![Finding::new(
                Code::SecretValue,
                "elsewhere",
                "other/place.t",
            )],
        }
    }
    fn seed(&self) -> Vec<SeedFile> {
        Vec::new()
    }
}
