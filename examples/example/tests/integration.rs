//! End-to-end integration tests for `#[subcrate]`, run against the real build
//! pipeline (the `example` package's `build.rs` compiles the subcrates).

use subcrate::subcrate;

// A `#[subcrate]` declared *inside an integration test file*. This proves the
// build helper discovers subcrates under `tests/`, and lets us check crate
// boundaries directly: `crate::` resolves to this subcrate, not to the test
// crate.
#[subcrate]
pub mod boundary {
    pub struct Marker;

    pub fn make() -> crate::Marker {
        crate::Marker
    }

    pub fn value() -> usize {
        7
    }
}

#[test]
fn library_and_nested_subcrates_resolve_by_original_path() {
    assert_eq!(example::my_crate::calc(), 32);
    assert_eq!(example::nested::inner::answer(), 42);
    assert_eq!(example::calculate(), 74);
}

#[test]
fn proc_macro_subcrate_expands() {
    assert_eq!(example::my_macro::mac!(), 42);
}

#[test]
fn one_crate_exports_both_a_proc_macro_and_normal_items() {
    // Normal function from the same crate...
    assert_eq!(example::calculate(), 74);
    // ...and a proc-macro, re-exported to that crate's root.
    assert_eq!(example::mac!(), 42);
}

#[test]
fn subcrate_declared_under_tests_dir_is_compiled() {
    // `crate::Marker` inside `boundary` resolved to the subcrate's own root,
    // confirming a genuine crate boundary.
    let _marker: boundary::Marker = boundary::make();
    assert_eq!(boundary::value(), 7);
}
