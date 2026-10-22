//! Proves the build helper discovers `#[subcrate]` modules under `examples/`
//! (in addition to `src/`, `tests/`, and `benches/`). Building this example at
//! all requires that discovery: otherwise the macro would error that the module
//! was never compiled.

use subcrate::subcrate;

#[subcrate]
pub mod gadget {
    pub fn n() -> u32 {
        5
    }
}

fn main() {
    assert_eq!(gadget::n(), 5);
    println!("discovered example ok");
}
