//! Test fixture for subcrates that use their owner's dependencies: a crate that
//! depends on another one (`leaf-base`), so a subcrate using it needs that
//! transitive dependency to be found too.

/// A type whose identity has to be the same wherever it is named from.
#[derive(Debug, PartialEq, Eq)]
pub struct Marker(pub u32);

/// A value computed with the help of the dependency.
pub fn value() -> u32 {
    leaf_base::base() * 2
}
