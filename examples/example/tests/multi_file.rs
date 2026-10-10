//! A subcrate whose body is split across several files that reference one
//! another: the root imports `a` and `b`, and `b` uses `a` through a
//! `crate::`-relative path — all resolved inside the subcrate's own crate.
//!
//! `mod foo;` inside a subcrate needs nightly Rust (non-inline modules in
//! proc-macro input are unstable, `E0658`, on stable), so this file is gated
//! behind the `nightly` feature and run by the nightly CI job.
#![cfg(feature = "nightly")]

use subcrate::subcrate;

#[subcrate]
pub mod multifile {
    mod a; // tests/multifile/a.rs
    mod b; // tests/multifile/b.rs  (references `a`)

    pub fn total() -> u32 {
        a::va() + b::vb()
    }
}

#[test]
fn multi_file_subcrate_with_cross_references() {
    // a::va() = 10 ; b::vb() = crate::a::va() + 20 = 30
    assert_eq!(multifile::total(), 40);
}
