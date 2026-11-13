//! Test fixture for target auto-discovery switched off (`autotests = false` and
//! the like). The build helper reads the targets this manifest declares; the files
//! in `tests/`, `benches/`, `examples/` and `src/bin/` named `foreign*.rs` belong to
//! no target of it (each has a `#[subcrate]` that cannot be resolved, which fails the
//! build if the helper reads it).

use subcrate::subcrate;

#[subcrate]
pub mod in_lib {
    pub fn value() -> u32 {
        1
    }
}
