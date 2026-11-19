//! A declared `[[bench]]` at its default location (`harness = false`, so it is a
//! plain program).
use subcrate::subcrate;

#[subcrate]
pub mod in_bench {
    pub fn value() -> u32 {
        4
    }
}

fn main() {
    assert_eq!(in_bench::value(), 4);
}
