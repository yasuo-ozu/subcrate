//! Tests for per-subcrate configuration knobs: `#[subcrate(edition = "…")]` and
//! crate-root inner attributes (`#![…]`).

use subcrate::subcrate;

// Edition override *and* inner attributes together: `array.into_iter()` yields
// `&T` only under edition <= 2018 (so `*x` type-checks), and
// `#![allow(array_into_iter)]` silences that edition's compatibility lint — so
// this compiling at all proves the `edition = "2018"` override reached the
// subcrate's `rustc` invocation.
#[subcrate(edition = "2018")]
pub mod edition_2018 {
    #![allow(array_into_iter)]
    pub fn sum() -> i32 {
        let arr = [1i32, 2, 3];
        let mut s = 0;
        for x in arr.into_iter() {
            s += *x;
        }
        s
    }
}

// A crate-root inner attribute forwarded to the subcrate's crate root: it allows
// the otherwise-warned non-snake-case function name.
#[subcrate]
pub mod inner_attrs {
    #![allow(non_snake_case)]
    pub fn Value() -> i32 {
        9
    }
}

#[test]
fn per_subcrate_edition_override() {
    assert_eq!(edition_2018::sum(), 6);
}

#[test]
fn crate_root_inner_attributes_apply() {
    assert_eq!(inner_attrs::Value(), 9);
}
