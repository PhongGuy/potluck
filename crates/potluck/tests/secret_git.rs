//! Ref lines, and the plumbing calls against a scratch repository.

use potluck::secret_check::git::{ParseError, RefUpdate, Sha};

use crate::common::scratch::Scratch;

const ZERO: &str = "0000000000000000000000000000000000000000";

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

#[test]
fn ref_update_lines_parse_and_a_zero_new_is_a_delete() {
    let a = "a".repeat(40);
    let u = RefUpdate::parse_line(&format!("{ZERO} {a} refs/heads/main\n")).unwrap();
    assert!(u.old().is_zero());
    assert!(!u.is_delete());
    assert_eq!(u.new().as_str(), a);
    assert_eq!(u.name(), "refs/heads/main");
    assert!(
        RefUpdate::parse_line(&format!("{a} {ZERO} refs/heads/gone"))
            .unwrap()
            .is_delete()
    );
    assert!(matches!(
        RefUpdate::parse_line("x y").unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(&format!("{ZERO} {a} refs/heads/two words")).unwrap_err(),
        ParseError::Shape { .. }
    ));
    assert!(matches!(
        RefUpdate::parse_line(&format!("{ZERO} nothex refs/heads/x")).unwrap_err(),
        ParseError::NotHex { .. }
    ));
    assert_eq!(
        Sha::parse(&"F".repeat(64)).unwrap().as_str(),
        "f".repeat(64),
        "SHA-256 width, folded to lower case"
    );
    assert!(Sha::parse("abcdef").is_err(), "wrong width");
    assert_eq!(sha(ZERO).to_string(), ZERO);
}

#[test]
fn scratch_stages_objects_without_refs_and_publishes_with_them() {
    let s = Scratch::new();
    s.write("README", b"seed\n");
    let tip = s.commit("seed");
    assert_eq!(s.stage("main"), tip);
    assert_eq!(
        s.bare_git(&["for-each-ref"]),
        "",
        "staged: no ref points at the objects"
    );
    assert_eq!(
        s.bare_git(&["cat-file", "-t", &tip]),
        "commit",
        "staged: the objects are there"
    );
    s.publish("main");
    assert_eq!(s.bare_git(&["rev-parse", "refs/heads/main"]), tip);
}
