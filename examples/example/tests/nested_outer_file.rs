//! A nested module pulled in from an outer file (`mod inner;`) inside a
//! `#[subcrate]`. The build helper inlines the file before compiling.
//!
//! This exercises the `mod foo;` form, which needs a Rust newer than the 1.56
//! MSRV (a non-inline module in proc-macro input was `E0658` on 1.56), so it is
//! covered by the stable CI jobs. The committed MSRV job only `cargo check`s
//! (no `--all-targets`), which runs the build script but does not compile this
//! test crate, so it stays green.

use subcrate::subcrate;

#[subcrate]
pub mod outer_file {
    mod inner; // resolves to tests/outer_file/inner.rs

    pub fn total() -> u32 {
        inner::value() + 1
    }
}

#[test]
fn external_file_module_is_inlined() {
    assert_eq!(outer_file::total(), 100);
}
