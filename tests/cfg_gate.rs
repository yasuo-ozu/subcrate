//! Regression for issue 3: `#[cfg(...)]` on a `#[subcrate]` module is honoured.
//!
//! A `#[subcrate]` module Cargo's configuration switches off must be neither built
//! nor checked — the real compilation removes it entirely, so the build helper has
//! to as well. This is what lets a feature-dependent compile-fail suite gate a case
//! on a feature.

// The attribute is used only by the gated modules below, so the import follows
// them under the same feature (the build helper reads this file's source either
// way, so it still finds and evaluates the gated modules' `#[cfg]`).
#[cfg(feature = "ui_case")]
use subcrate::subcrate;

// This UI-test subcrate is gated on `ui_case`, which is off by default. Its body
// errors (an undefined name) *only* when `ui_case` is on — the inner `#[cfg]`, like
// the outer one, is evaluated against the outer build's features (issue 2). So:
//
// * with `ui_case` on, the module is active, checked, and its error meets
//   `should_error`;
// * with `ui_case` off, the module is configured out and must be skipped. Were the
//   `#[cfg]` ignored and the module checked anyway (the issue-3 bug), its body would
//   compile with nothing to error on, and the unmet `should_error` would fail the
//   build.
#[cfg(feature = "ui_case")]
#[subcrate(should_error)]
mod gated_ui_case {
    pub fn f() {
        #[cfg(feature = "ui_case")]
        this_name_is_undefined();
    }
}

// A cfg-gated subcrate that *does* compile when active, to show the ordinary path:
// when `ui_case` is on it is a real, linkable crate; when off it is configured out
// (and so is the test that would use it).
#[cfg(feature = "ui_case")]
#[subcrate]
mod only_with_ui_case {
    pub fn value() -> u32 {
        7
    }
}

#[cfg(feature = "ui_case")]
#[test]
fn a_cfg_gated_subcrate_is_a_real_crate_when_the_feature_is_on() {
    assert_eq!(only_with_ui_case::value(), 7);
}

#[test]
fn a_cfg_gated_ui_subcrate_is_skipped_when_configured_out() {
    // The real check is at build time: with `ui_case` off, `gated_ui_case` must be
    // skipped, not checked (or the unmet `should_error` fails the build); with it
    // on, it must be checked and matched. Reaching this test means the build helper
    // evaluated the module's `#[cfg]` correctly either way.
    let _gated_on = cfg!(feature = "ui_case");
}
