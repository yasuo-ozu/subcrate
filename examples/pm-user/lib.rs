//! Test fixture for the workspace-member feature-unification case: a library that
//! depends on the proc-macro crate `pm`, so `proc-macro2` is in its graph. When a
//! subcrate links this crate with `#[subcrate(::pm_user)]`, the nested build has to
//! select the owner too, or `pm-user` is built with `proc-macro2/span-locations`
//! off (the main build has it on through `subcrate`) and gets a different crate
//! identity. See `tests/owner.rs`.

pub use pm::identity;

/// A type whose identity has to be the same wherever it is named from.
#[derive(Debug, PartialEq, Eq)]
pub struct Marker(pub u32);

/// A value to check the right crate was linked.
pub fn value() -> u32 {
    11
}
