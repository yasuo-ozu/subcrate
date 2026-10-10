//! Integration tests for the `subcrate::build` API that do not require a build
//! script (the root `subcrate` package cannot run its own build helper, since a
//! package may not depend on itself).

/// `compile()` must fail with a clear, actionable error — not panic — when it is
/// called outside a Cargo build script (`OUT_DIR` unset). This is the companion
/// to the `#[subcrate]` "build helper not invoked" compile error.
#[test]
fn compile_outside_build_script_reports_clear_error() {
    // This test binary has no build script, so `OUT_DIR` is not set.
    let error =
        subcrate::build::compile().expect_err("compile() should fail without a build script");
    let message = error.to_string();
    assert!(
        message.contains("build script"),
        "error should mention the build script, got: {}",
        message
    );
}

/// The same holds for the explicit builder entry point.
#[test]
fn config_from_env_requires_build_script_env() {
    assert!(
        subcrate::build::Config::from_env().is_err(),
        "Config::from_env() should fail without build-script environment"
    );
}
