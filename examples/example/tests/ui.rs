//! UI testing: asserting what the compiler says about a subcrate.
//!
//! A `#[subcrate]` carrying any `should_error`, `should_warn` or `should_spanned`
//! argument is a **UI test**. The build helper compiles it, then checks the
//! diagnostics `rustc` produced against those expectations. If an expectation is
//! not met the *build* fails, with the unmet expectation and the real diagnostics;
//! so these assertions are checked by a plain `cargo build`/`cargo test`, with no
//! runtime test needed. (`should_spanned` compares the underlined code with
//! whitespace removed, so spacing does not matter.)

use subcrate::subcrate;

// An error test: this subcrate is *expected* to fail to compile. `does_not_exist`
// is undefined, so rustc reports E0425 and underlines the name. Because the crate
// never produces an artifact, the macro expands `fails` to an empty module.
#[subcrate(should_error = "cannot find value", should_spanned = "does_not_exist")]
pub mod fails {
    pub fn broken() -> u32 {
        does_not_exist
    }
}

// A warning test: this subcrate *does* compile (and is bound as a real crate, so
// `warns::value()` is callable below), but it must emit the unused-variable
// warning, which underlines `unused`. The asserted warning is not forwarded as a
// build warning.
#[subcrate(should_warn = "unused variable", should_spanned = "unused")]
pub mod warns {
    pub fn value() -> u32 {
        let unused = 5;
        7
    }
}

// Bare forms assert only the direction, not a specific message: `should_error`
// that it fails to compile at all, `should_warn` that it emits some warning.
#[subcrate(should_error)]
pub mod fails_somehow {
    pub fn broken() -> u32 {
        1 + "nope"
    }
}

#[subcrate(should_warn)]
pub mod warns_somehow {
    pub fn value() -> u32 {
        let whatever = 1;
        2
    }
}

#[test]
fn a_passing_warn_test_subcrate_is_a_real_usable_crate() {
    // The UI assertions are verified at build time; this only shows that a
    // warn-test subcrate is still an ordinary, linkable crate.
    assert_eq!(warns::value(), 7);
    assert_eq!(warns_somehow::value(), 2);
}
