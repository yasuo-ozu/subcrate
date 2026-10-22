//! Test fixture for subcrates that use their owner's dependencies: the crate at
//! the bottom of the chain `example -> leaf -> leaf-base`. Its package name has a
//! dash, so `#[subcrate(::leaf_base)]` also exercises the dash-to-underscore
//! lookup.

/// The number the chain is built on.
pub fn base() -> u32 {
    5
}
