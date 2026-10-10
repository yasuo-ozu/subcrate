//! Nested modules defined inline (in the same file) inside a `#[subcrate]`.

use subcrate::subcrate;

#[subcrate]
pub mod same_file {
    pub mod a {
        pub mod b {
            pub fn deep() -> u32 {
                7
            }
        }
        pub fn mid() -> u32 {
            b::deep() + 1
        }
    }

    mod hidden {
        pub fn n() -> u32 {
            100
        }
    }

    pub fn total() -> u32 {
        a::mid() + a::b::deep() + hidden::n()
    }
}

#[test]
fn inline_nested_modules_resolve() {
    assert_eq!(same_file::total(), 115); // 8 + 7 + 100
}

#[test]
fn deep_nested_path_is_reachable_from_outside() {
    assert_eq!(same_file::a::b::deep(), 7);
}
