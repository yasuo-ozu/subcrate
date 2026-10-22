//! Consumer-side tests: a `tests/` file is its own crate that links `subcrate-tests`,
//! so these exercise `subcrate-tests`'s *publicly exported* subcrates from outside the
//! defining module — the role previously held by the separate `downstream`
//! package.
//!
//! This covers the public API and transitive resolution of the subcrate
//! artifacts across a crate boundary. It does not re-assert Cargo's forwarding
//! of a dependency's build-script search path to a separate *package*, which is
//! Cargo's own behavior.

#[test]
fn library_subcrates_usable_by_a_consumer() {
    assert_eq!(subcrate_tests::calculate(), 74);
    assert_eq!(subcrate_tests::my_crate::calc(), 32);
    assert_eq!(subcrate_tests::nested::inner::answer(), 42);
}

#[test]
fn subcrate_depending_on_a_subcrate_usable_by_a_consumer() {
    // `doubled` links against `my_crate`; a downstream crate that uses `doubled`
    // must find `my_crate`'s artifact too (it is recorded in `doubled`'s metadata).
    assert_eq!(subcrate_tests::doubled::calc(), 64);
}

#[test]
fn proc_macro_subcrate_usable_by_a_consumer() {
    // Both the module path and the crate-root re-export resolve.
    assert_eq!(subcrate_tests::my_macro::mac!(), 42);
    assert_eq!(subcrate_tests::mac!(), 42);
}
