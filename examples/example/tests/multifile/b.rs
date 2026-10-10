pub fn vb() -> u32 {
    // `crate::` refers to the subcrate's own crate root, where `a` lives.
    crate::a::va() + 20
}
