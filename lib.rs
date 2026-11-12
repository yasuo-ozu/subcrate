// Without the `build` feature only the attribute is left, which needs nothing from `std`,
// so a `no_std` package can depend on it (e.g. for `x86_64-unknown-none`).
#![cfg_attr(not(feature = "build"), no_std)]
#![doc = include_str!("README.md")]

/// Compiles the annotated inline module as a separate crate. See the
/// [crate-level documentation](crate) for the full contract.
pub use subcrate_macros::subcrate;

#[cfg(feature = "build")]
pub mod build;
