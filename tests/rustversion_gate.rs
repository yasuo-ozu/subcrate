//! Toolchain-conditional subcrates, gated with [`rustversion`].
//!
//! Putting a `#[rustversion::…]` selector above `#[subcrate]` makes the subcrate
//! available only on a toolchain that satisfies the selector — and, crucially, a
//! toolchain that does *not* satisfy it must still build. The build helper does
//! not evaluate the selector itself: `rustversion` removes the module from this
//! crate on a non-matching toolchain, and the helper treats a `#[rustversion]`-
//! gated subcrate that fails to compile as "not for this toolchain", skipping it
//! instead of failing the build.
//!
//! So this file compiles on every channel. The gated module below is a real,
//! usable crate only on nightly; on stable/beta it is absent, and its nightly-only
//! body never breaks the build.

use subcrate::subcrate;

// An ordinary, ungated subcrate beside the gated one: it is a real crate on every
// channel (and keeps `use subcrate::subcrate;` in use on stable, where `rustversion`
// removes the gated module below).
#[subcrate]
pub mod always {
    pub fn value() -> u32 {
        7
    }
}

#[test]
fn an_ungated_subcrate_is_available_on_every_toolchain() {
    assert_eq!(always::value(), 7);
}

// `#![feature(..)]` is rejected on stable and beta (E0554), so on those channels
// the helper skips this subcrate rather than failing — and `rustversion` removes
// `nightly_only` from the crate entirely. On nightly the body compiles, so it is
// built as a real crate and `nightly_only::answer()` is callable (see the test).
#[rustversion::nightly]
#[subcrate]
pub mod nightly_only {
    #![feature(test)]
    // Using the `test` crate exercises the feature (so no `unused_features`
    // warning, which `-D warnings` would otherwise turn into a build error and
    // wrongly skip this subcrate on nightly).
    extern crate test;
    pub fn answer() -> u32 {
        test::black_box(42)
    }
}

#[rustversion::nightly]
#[test]
fn the_gated_subcrate_is_a_real_crate_on_nightly() {
    assert_eq!(nightly_only::answer(), 42);
}

// The point of this target is that it builds at all: on stable/beta the gated
// subcrate above is skipped (its nightly-only body cannot compile) without failing
// the build, and on nightly it is a real crate. Either way we get here.
#[test]
fn the_build_succeeds_on_every_toolchain() {}
