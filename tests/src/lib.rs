//! A dedicated package for `subcrate`'s own end-to-end tests.
//!
//! Its `build.rs` runs `subcrate::build::compile()`, so the
//! `#[subcrate(should_*)]` UI tests in `ui.rs` are compiled and checked at build
//! time — something the root `subcrate` crate cannot do for itself, since a
//! package may not depend on itself. `args.rs` and `build_api.rs` exercise the
//! `subcrate_core` / `subcrate::build` APIs directly.
//!
//! The flat test files are reached as `tests/tests/*.rs` through the `tests`
//! symlink (-> this directory), the conventional location the build helper
//! scans for `#[subcrate]` sources.
