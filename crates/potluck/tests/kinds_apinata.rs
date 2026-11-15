//! The apinata kind: environment files, the YAML walk and the seed.

use potluck::kinds::apinata::{is_environment_file, seed};
use potluck::kinds::generic;

const SEED_COLLECTION: &[u8] = b"opencollection: 1.0.0\n\ninfo:\n  name: Workspace\nbundled: false\nextensions:\n  bruno:\n    ignore:\n      - node_modules\n      - .git\n";

#[test]
fn environment_files_at_any_depth_are_checked_and_nothing_else_is() {
    for p in [
        "environments/dev.yml",
        "collections/api/environments/dev.yml",
    ] {
        assert!(is_environment_file(p), "{p}");
    }
    for p in [
        "environments/nested/dev.yml",
        "environments/dev.yaml",
        "my-environments/dev.yml",
        "dev.yml",
        "environments.yml",
        "environments/dev.yml.bak",
        "-",
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
