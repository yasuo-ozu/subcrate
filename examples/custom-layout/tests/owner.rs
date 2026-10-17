//! A subcrate in a test depends on the owner library, which is not at `src/lib.rs`
//! but at the `[lib] path` of the manifest (`weird/the_lib.rs`).
use subcrate::subcrate;

#[subcrate(::custom_layout)]
pub mod uses_owner {
    pub fn twice() -> u32 {
        custom_layout::get() * 2
    }
}

#[test]
fn a_subcrate_depends_on_a_library_at_a_custom_path() {
    assert_eq!(uses_owner::twice(), 14);
}
