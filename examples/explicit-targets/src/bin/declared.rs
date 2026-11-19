//! A declared `[[bin]]` at its default location (`src/bin/<name>.rs`).
use subcrate::subcrate;

#[subcrate]
pub mod in_bin {
    pub fn value() -> u32 {
        6
    }
}

fn main() {
    assert_eq!(in_bin::value(), 6);
}
