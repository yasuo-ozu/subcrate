//! A subcrate whose body is split across several files that reference one
//! another: the root imports `a` and `b`, and `b` uses `a` through a
//! `crate::`-relative path — all resolved inside the subcrate's own crate.
//!
//! Like the other `mod foo;` tests, this needs a Rust newer than the 1.56 MSRV.

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
