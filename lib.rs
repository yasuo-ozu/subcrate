// Without the `build` feature only the attribute is left, which needs nothing from `std`,
// so a `no_std` package can depend on it (e.g. for `x86_64-unknown-none`).
#![cfg_attr(not(feature = "build"), no_std)]
#![doc = include_str!("README.md")]

/// Compiles the annotated inline module as a separate crate. See the
/// [crate-level documentation](crate) for the full contract.
///
/// A `#[subcrate(should_error = "…")]` module in a package with **no** build script
/// is checked at test time instead: it expands to a `#[test]` that compiles the body
/// and asserts its diagnostics (see [`ui`]), so a compile-fail suite needs `subcrate`
/// only under `[dev-dependencies]`. Every other subcrate still requires the build
/// helper. See the [crate-level documentation](crate).
pub use subcrate_macros::subcrate;

#[cfg(feature = "build")]
pub mod build;

#[cfg(feature = "build")]
pub mod ui;
