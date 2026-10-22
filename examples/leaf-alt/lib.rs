//! Test fixture for subcrates that use a *renamed* dependency of their owner: the
//! package `leaf-alt`, which `examples/example` depends on as `alt`
//! (`alt = { package = "leaf-alt", … }`). Cargo refuses to list one package under
//! two names, so this is a package of its own rather than `leaf` renamed.

/// A type whose identity has to be the same wherever it is named from.
#[derive(Debug, PartialEq, Eq)]
pub struct Marker(pub u32);

/// A value to check the right crate was linked.
pub fn value() -> u32 {
    7
}
