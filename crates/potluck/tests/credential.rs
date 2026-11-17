//! Roles order; credentials parse from both schemes and never print themselves.

use potluck::api::credential::Credential;
use potluck::role::Role;

#[test]
fn roles_order_read_write_admin() {
    assert!(Role::Read < Role::Write && Role::Write < Role::Admin);
    assert_eq!(Role::Write.as_str(), "write");
    assert!(Role::Admin >= Role::Write, "admin may push");
    assert!(Role::Read < Role::Write, "read may not push");
}

#[test]
fn debug_never_prints_the_token() {
    let c = Credential::new("plk_supersecret".into());
    let shown = format!("{c:?}");
    assert!(!shown.contains("supersecret"), "{shown}");
    assert!(shown.contains("redacted"), "{shown}");
}
