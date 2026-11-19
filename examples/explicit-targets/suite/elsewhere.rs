//! A `[[test]]` with a `path` outside `tests/`: its subcrate is found.
use subcrate::subcrate;

#[subcrate]
pub mod in_suite {
    pub fn value() -> u32 {
        2
    }
}

#[test]
fn a_declared_test_with_a_path_is_a_root() {
    assert_eq!(in_suite::value(), 2);
    assert_eq!(explicit_targets::in_lib::value(), 1);
}
