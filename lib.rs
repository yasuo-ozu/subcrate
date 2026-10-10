#![doc = include_str!("README.md")]

/// Compiles the annotated inline module as a separate crate. See the
/// [crate-level documentation](crate) for the full contract.
pub use subcrate_macros::subcrate;

#[cfg(feature = "build")]
pub mod build;
