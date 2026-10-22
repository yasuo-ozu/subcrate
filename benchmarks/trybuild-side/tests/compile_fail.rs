// The trybuild counterpart of the subcrate-side UI tests: every `tests/ui/*.rs`
// case must fail to compile with the diagnostics captured in its `.stderr`.
#[test]
fn ui() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
