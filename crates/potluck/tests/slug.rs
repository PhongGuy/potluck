//! `Slug` accepts exactly `^[a-z0-9][a-z0-9-]{0,62}$`.

use potluck::slug::{Slug, SlugError};

#[test]
fn valid_names_round_trip() {
    for name in ["a", "0", "core", "api-v2", "a-", &"x".repeat(63)] {
        let slug = Slug::parse(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(slug.as_str(), name);
        assert_eq!(slug.to_string(), name);
        assert_eq!(
            name.parse::<Slug>().unwrap(),
            slug,
            "FromStr agrees with parse"
        );
    }
}

#[test]
fn length_is_1_to_63() {
    assert_eq!(Slug::parse("").unwrap_err(), SlugError::Length);
    assert_eq!(Slug::parse(&"x".repeat(64)).unwrap_err(), SlugError::Length);
}

#[test]
fn only_lower_case_digits_and_inner_hyphens() {
    for name in ["-a", "A", "a_b", "a.b", "a/b", "..", "a b", "ä", "a\0"] {
        assert_eq!(
            Slug::parse(name).unwrap_err(),
            SlugError::Charset,
            "{name:?}"
        );
    }
}
