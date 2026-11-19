//! Test fixture for subcrates that link a dependency which is itself backed by
//! subcrates (the shape of a crate with inline proc-macros, `announce`): the build
//! script of this package compiles two subcrates, one of them a proc-macro that
//! uses `::leaf` (so that build script runs a nested build of its own).
//!
//! A package that links `::backed` from one of its own subcrates builds this
//! package in *its* nested build. That has to compile these subcrates there (it
//! must not mistake itself for the package that started the nested build), and the
//! subcrate that links `backed` has to find them (`backed`'s metadata refers to
//! them). See `tests/transitive.rs` and `member-test`.

use subcrate::subcrate;

#[subcrate(proc_macro = true, ::leaf)]
pub mod macros {
    use proc_macro::TokenStream;

    /// `leaf::value()`, as a literal.
    #[proc_macro]
    pub fn leafy(_: TokenStream) -> TokenStream {
        leaf::value().to_string().parse().unwrap()
    }
}

#[subcrate]
pub mod plain {
    pub fn value() -> u32 {
        7
    }
}

pub use macros::leafy;

pub fn value() -> u32 {
    plain::value() + 1
}
