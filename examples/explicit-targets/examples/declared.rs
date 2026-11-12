//! A declared `[[example]]` at its default location.
use subcrate::subcrate;

#[subcrate]
pub mod in_example {
    pub fn value() -> u32 {
        5
    }
}

fn main() {
    assert_eq!(in_example::value(), 5);
}
