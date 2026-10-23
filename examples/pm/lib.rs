//! Test fixture for the workspace-member feature-unification case: a proc-macro
//! crate that depends on `proc-macro2`, so a library depending on it carries
//! `proc-macro2` in its graph on the host side. `subcrate`'s own build dependency
//! turns on `proc-macro2/span-locations` there, which is what would give a member
//! built without the owner in the nested selection a different hash — the bug
//! `tests/owner.rs` guards against.

use proc_macro::TokenStream;

/// Returns the input tokens unchanged, round-tripping through `proc-macro2` so this
/// crate genuinely pulls it in.
#[proc_macro]
pub fn identity(input: TokenStream) -> TokenStream {
    proc_macro2::TokenStream::from(input).into()
}
