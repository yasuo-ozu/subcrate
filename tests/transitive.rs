//! A subcrate that links a dependency of the package that is itself backed by
//! subcrates (`examples/backed`: a library, and a proc-macro that uses `::leaf`, as
//! subcrates), the way a crate with inline proc-macros is used. This package uses its
//! own library too (`owner.rs`), so the nested build is the one that builds the owner's
//! library as well, with `SUBCRATE_NESTED` set but not `SUBCRATE_HARVEST_ONLY`;
//! `member-test` covers the other kind.
//!
//! `backed`'s build script runs in that nested build. It must compile its subcrates
//! (the environment is for this package's build script, not for it), and the subcrate
//! here must find the crates `backed` refers to, which are in `backed`'s own `OUT_DIR`.

use subcrate::subcrate;

#[subcrate(::backed)]
pub mod uses_backed {
    pub fn macro_value() -> u32 {
        backed::leafy!() + 1
    }

    pub fn value() -> u32 {
        backed::value() + 1
    }
}

// Through a sibling: `via_sibling` is built on `uses_backed`, so it finds those crates
// as well.
#[subcrate(uses_backed)]
pub mod via_sibling {
    pub fn value() -> u32 {
        uses_backed::macro_value() + uses_backed::value()
    }
}

#[test]
fn a_subcrate_can_link_a_dependency_that_is_backed_by_subcrates() {
    assert_eq!(uses_backed::macro_value(), 10 + 1);
    assert_eq!(uses_backed::value(), 7 + 1 + 1);
    assert_eq!(via_sibling::value(), 11 + 9);
}

#[test]
fn the_package_itself_sees_the_same_dependency() {
    assert_eq!(backed::value(), 7 + 1);
    assert_eq!(backed::leafy!(), 10);
}
