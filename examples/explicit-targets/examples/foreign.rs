//! Not a target of this package (auto-discovery is off), so no `cargo` command builds
//! it; it is in the tree for the build helper to leave alone. Read as a crate root,
//! the dependency below cannot be resolved and the build fails.
#[subcrate(not_in_this_package)]
pub mod unresolved {}
