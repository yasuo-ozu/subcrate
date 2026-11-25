//! `#[subcrate(should_error …)]` without a build script. This package has no
//! `build.rs`, so `subcrate::build::compile()` never runs and the attribute falls
//! back to the test-time harness (`subcrate::ui`): each module below becomes its own
//! `#[test]` that compiles the body and asserts its diagnostics. `subcrate` is only a
//! `[dev-dependencies]` entry here.

use subcrate::subcrate;

// A type error: the expected code and the underlined literal, no dependencies.
#[subcrate(should_error = "E0308", should_spanned = "\"x\"")]
mod mismatched_type {
    pub fn f() {
        let _: u8 = "x";
    }
}

// A bare `should_error`: the body must simply fail to compile.
#[subcrate(should_error)]
mod does_not_compile {
    pub fn f() {
        nonexistent_function();
    }
}

// Links an external dependency (`leaf`, a dev-dependency of this package). The error
// is `E0308` on `leaf::value()` — a `u32` assigned to a `u8` — which can only arise
// if `leaf` actually resolved and linked.
#[subcrate(::leaf, should_error = "E0308")]
mod uses_a_dependency {
    pub fn f() {
        let _: u8 = leaf::value();
    }
}

// The harness must actually *fail* when an expectation is unmet, not pass vacuously:
// a body that compiles cleanly cannot satisfy `should_error`.
#[test]
fn an_unmet_expectation_fails_the_test() {
    let case = subcrate::ui::Case {
        name: "self_check",
        source_file: file!(),
        body: "pub fn ok() -> u32 { 1 }",
        deps: &[],
        edition: None,
        expect: subcrate::ui::Expect {
            should_errors: &[Some("E0308")],
            should_warns: &[],
            should_spanned: &[],
        },
    };
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| subcrate::ui::run(&case));
    std::panic::set_hook(prev);
    assert!(
        result.is_err(),
        "the harness should fail a case whose body did not meet its expectation"
    );
}
