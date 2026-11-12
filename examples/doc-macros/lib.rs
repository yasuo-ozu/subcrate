//! Test fixture: the three kinds of proc-macro, defined in one `proc_macro` subcrate and
//! re-exported at this crate's root. `cargo doc --no-deps` has to document the package
//! (`tests/doc_standin.rs` in `subcrate-tests` runs it and looks at the pages), so the
//! `cfg(doc)` stand-in of the subcrate must cover each kind.
//!
//! Under `cargo doc` an attribute or a derive from the stand-in cannot be applied, so the
//! package's own uses are guarded with `not(doc)`; `tests/use_macros.rs` uses them freely.

use subcrate::subcrate;

#[subcrate(proc_macro = true)]
pub mod macros {
    use proc_macro::TokenStream;

    /// Function-like macro: expands to `1`.
    ///
    /// DOC-MARKER-FUNC
    ///
    /// ```
    /// assert_eq!(doc_macros::my_func!(), 1);
    /// ```
    #[proc_macro]
    pub fn my_func(_: TokenStream) -> TokenStream {
        "1".parse().unwrap()
    }

    /// Not documented: `#[doc(hidden)]` stays hidden in the stand-in too.
    #[doc(hidden)]
    #[proc_macro]
    pub fn hidden_func(_: TokenStream) -> TokenStream {
        "2".parse().unwrap()
    }

    /// Attribute macro: returns the item unchanged.
    ///
    /// DOC-MARKER-ATTR
    ///
    /// ```
    /// #[doc_macros::my_attr]
    /// struct Item;
    /// ```
    #[proc_macro_attribute]
    pub fn my_attr(_: TokenStream, item: TokenStream) -> TokenStream {
        item
    }

    /// Derive macro: defines `pub fn derived() -> u32` next to the type.
    ///
    /// DOC-MARKER-DERIVE
    ///
    /// ```
    /// #[derive(doc_macros::MyDerive)]
    /// struct Item;
    /// ```
    #[proc_macro_derive(MyDerive, attributes(helper))]
    pub fn derive_my_derive(_: TokenStream) -> TokenStream {
        "pub fn derived() -> u32 { 7 }".parse().unwrap()
    }
}

pub use macros::{hidden_func, my_attr, my_func, MyDerive};

/// A type that applies the attribute and the derive, guarded for `cargo doc`.
#[cfg_attr(not(doc), my_attr)]
#[cfg_attr(not(doc), derive(MyDerive))]
pub struct Used {
    /// A field with the derive's helper attribute.
    #[cfg_attr(not(doc), helper)]
    pub field: u32,
}
