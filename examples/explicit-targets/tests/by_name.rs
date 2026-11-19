//! A `[[test]]` without a `path`, so at its default location: still a root.
use subcrate::subcrate;

#[subcrate]
pub mod in_default_location {
    pub fn value() -> u32 {
        3
    }
}

#[test]
fn a_declared_test_at_its_default_location_is_a_root() {
    assert_eq!(in_default_location::value(), 3);
}
