//! A library deliberately placed at `weird/the_lib.rs` (via `[lib] path`), not
//! `src/lib.rs`, to prove the build helper discovers `#[subcrate]` modules through
//! a target path declared in `Cargo.toml` — in a workspace member, no less.
use subcrate::subcrate;

#[subcrate]
pub mod inner {
    pub fn value() -> u32 {
        7
    }
}

pub fn get() -> u32 {
    inner::value()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_subcrate_in_a_custom_lib_path_is_discovered() {
        // If the custom `[lib] path` were not read, `inner` would be undiscovered
        // and `#[subcrate]` would fail to compile; reaching this proves it works.
        assert_eq!(super::get(), 7);
    }
}
