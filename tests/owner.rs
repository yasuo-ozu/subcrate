//! Subcrates using their *owner's* dependencies and the owner crate itself:
//! `#[subcrate(::leaf)]` links the crate `leaf`, a dependency of this package
//! (`examples/leaf`, which depends on `examples/leaf-base`), and
//! `#[subcrate(::subcrate_tests)]` links this package's own library.
//!
//! The build helper has Cargo build those crates into a separate target
//! directory (the main one is locked by the build in progress) and passes them to
//! the subcrates with `--extern`. These tests check the result is the *same*
//! crate the parent uses, not a lookalike.
//!
//! Using the owner crate from a subcrate in `src/lib.rs`, an unknown `::name`, and
//! the like are build errors, which cannot be asserted from a passing test; they
//! are covered by the unit tests in `build.rs` (`subcrate::build`) instead.

use std::any::TypeId;

use subcrate::subcrate;

// -- A dependency of the owner ---------------------------------------------------

// `leaf` is visible inside `uses_leaf` under its own name.
#[subcrate(::leaf)]
pub mod uses_leaf {
    use std::any::TypeId;

    pub fn value() -> u32 {
        leaf::value() + 1
    }

    pub fn marker_type() -> TypeId {
        TypeId::of::<leaf::Marker>()
    }
}

#[test]
fn a_subcrate_can_call_a_dependency_of_its_owner() {
    // `leaf::value()` is `leaf_base::base() * 2`, so this also needed `leaf`'s own
    // dependency to be found.
    assert_eq!(uses_leaf::value(), 5 * 2 + 1);
    assert_eq!(leaf::value(), 10);
}

// A type from the dependency has to be the same type wherever it is named from:
// the subcrate is linked against the very crate this one is (built by Cargo into
// another directory, but identical), not a lookalike. It is checked with `TypeId`s,
// at run time, rather than by passing a `leaf::Marker` across: `cargo check` (and
// so clippy) builds a different copy of `leaf` than `cargo build` does, so only
// there would such code not type-check, while the values compared here would
// merely never run.
#[test]
fn an_owner_dependency_has_one_crate_identity_across_the_boundary() {
    assert_eq!(uses_leaf::marker_type(), TypeId::of::<leaf::Marker>());
}

// -- A package name with a dash ---------------------------------------------------

// `leaf-base` is a package; its crate (and so `::leaf_base`) has an underscore.
#[subcrate(::leaf_base)]
pub mod uses_leaf_base {
    pub fn base() -> u32 {
        leaf_base::base() + 100
    }
}

#[test]
fn a_dash_in_the_package_name_is_an_underscore_in_the_path() {
    assert_eq!(uses_leaf_base::base(), 105);
}

// -- A renamed dependency ----------------------------------------------------------

// This package depends on the package `leaf-alt` under the name `alt`
// (`alt = { package = "leaf-alt", … }` in its `Cargo.toml`), so `alt` is what its own
// code calls the crate, and what a subcrate writes: `::alt`.
#[subcrate(::alt)]
pub mod uses_renamed {
    use std::any::TypeId;

    pub fn value() -> u32 {
        alt::value() + 1
    }

    pub fn marker_type() -> TypeId {
        TypeId::of::<alt::Marker>()
    }
}

#[test]
fn a_subcrate_can_use_a_dependency_its_owner_renamed() {
    assert_eq!(uses_renamed::value(), 7 + 1);
    // Through the parent's own name for it, the same crate gives the same value.
    assert_eq!(alt::value(), 7);
}

#[test]
fn a_renamed_dependency_has_one_crate_identity_across_the_boundary() {
    assert_eq!(uses_renamed::marker_type(), TypeId::of::<alt::Marker>());
}

// Renamed and not renamed dependencies together, and the alias syntax on a renamed one.
#[subcrate(a = ::alt, ::leaf)]
pub mod renamed_and_plain {
    pub fn f() -> u32 {
        a::value() + leaf::value()
    }
}

#[test]
fn a_renamed_dependency_can_be_used_next_to_others_and_under_an_alias() {
    assert_eq!(renamed_and_plain::f(), 7 + 10);
}

// -- Aliases, several dependencies, re-exports ---------------------------------------

// `alias = ::name` renames the dependency, and several can be combined.
#[subcrate(l = ::leaf, ::leaf_base)]
pub mod renamed {
    // A dependency is imported at the crate root, so it can be re-exported ...
    pub use l::Marker;

    pub fn f() -> u32 {
        l::value() + leaf_base::base()
    }

    pub fn marker_type() -> std::any::TypeId {
        std::any::TypeId::of::<Marker>()
    }

    // ... and reached from a nested module as `crate::alias`.
    pub mod inner {
        pub fn g() -> u32 {
            crate::l::value()
        }
    }
}

#[test]
fn an_owner_dependency_can_be_renamed_and_reexported() {
    assert_eq!(renamed::f(), 15);
    assert_eq!(renamed::inner::g(), 10);
    assert_eq!(renamed::marker_type(), TypeId::of::<leaf::Marker>());
}

#[test]
fn an_owner_dependency_crate_is_itself_re_exported_from_the_subcrate() {
    // The dependency is bound with `pub extern crate`, so the crate itself — not
    // only its items — is reachable through the subcrate's public path (`renamed::l`
    // is `leaf`). A private `extern crate` could not be re-exported this way.
    assert_eq!(renamed::l::value(), leaf::value());
}

// In edition 2015 the import is `extern crate … as …;` rather than a `use`.
#[subcrate(edition = "2015", ::leaf)]
pub mod legacy {
    pub fn f() -> u32 {
        leaf::value() + 3
    }
}

#[test]
fn an_owner_dependency_links_in_edition_2015_too() {
    assert_eq!(legacy::f(), 13);
}

// -- Mixed with subcrate dependencies ------------------------------------------------

// A subcrate can take both: a sibling subcrate and an owner dependency.
#[subcrate(uses_leaf, ::leaf)]
pub mod both {
    pub fn f() -> u32 {
        uses_leaf::value() + leaf::value()
    }
}

#[test]
fn a_subcrate_can_depend_on_a_subcrate_and_on_an_owner_dependency() {
    assert_eq!(both::f(), 11 + 10);
}

// A proc-macro subcrate can use an owner dependency while it expands.
#[subcrate(proc_macro, ::leaf)]
pub mod leaf_macro {
    use proc_macro::TokenStream;

    #[proc_macro]
    pub fn leaf_value(_: TokenStream) -> TokenStream {
        leaf::value().to_string().parse().unwrap()
    }
}

#[test]
fn a_proc_macro_subcrate_can_use_an_owner_dependency() {
    assert_eq!(leaf_macro::leaf_value!(), 10);
}

// -- The owner crate -----------------------------------------------------------------

// `::subcrate_tests` is this package's own library (`tests/src/lib.rs`), built
// separately for the subcrate. It is only allowed outside the library itself, as
// here in a test file.
#[subcrate(::subcrate_tests)]
pub mod uses_owner {
    pub fn total() -> usize {
        subcrate_tests::calculate() + 1
    }

    // The owner's own subcrates are reachable through it, and so is the macro its
    // proc-macro subcrate exports.
    pub fn from_a_subcrate() -> usize {
        subcrate_tests::my_crate::calc()
    }

    pub fn from_a_macro() -> usize {
        subcrate_tests::mac!()
    }
}

#[test]
fn a_subcrate_can_use_the_owner_crate() {
    // `calculate()` is `my_crate::calc() + nested::inner::answer()`: 32 + 42.
    assert_eq!(subcrate_tests::calculate(), 74);
    assert_eq!(uses_owner::total(), 75);
}

#[test]
fn the_owner_crates_own_subcrates_work_through_it() {
    assert_eq!(uses_owner::from_a_subcrate(), 32);
    assert_eq!(uses_owner::from_a_macro(), 42);
}

// A subcrate depending on a subcrate that uses the owner crate: it has to be
// compiled after it, and after the owner's own subcrates.
#[subcrate(uses_owner)]
pub mod above_owner {
    pub fn f() -> usize {
        uses_owner::total() * 2
    }
}

#[test]
fn a_subcrate_can_depend_on_a_subcrate_that_uses_the_owner_crate() {
    assert_eq!(above_owner::f(), 150);
}

// Both at once: the owner crate and the dependencies it was built against.
#[subcrate(::subcrate_tests, ::leaf)]
pub mod owner_and_leaf {
    pub fn f() -> usize {
        subcrate_tests::calculate() + leaf::value() as usize
    }
}

#[test]
fn the_owner_crate_and_an_owner_dependency_can_be_used_together() {
    assert_eq!(owner_and_leaf::f(), 84);
}
