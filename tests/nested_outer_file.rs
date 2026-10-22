//! A nested module pulled in from an outer file (`mod inner;`) inside a
//! `#[subcrate]`. The build helper inlines the file before compiling.
//!
//! The `mod foo;` form needs nightly Rust (non-inline modules in proc-macro
//! input are unstable, `E0658`, on stable), so this file is gated behind the
//! `nightly` feature and run by the nightly CI job. The build helper itself
//! compiles the subcrate fine everywhere (it inlines the file before `rustc`);
//! only the attribute-macro input in this test crate needs nightly.
#![cfg(feature = "nightly")]

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
