//! Subcrates depending on other subcrates: `#[subcrate(other)]` links the sibling
//! subcrate `other` into this one (`--extern other=…`), and the build helper
//! compiles the subcrates in dependency order.
//!
//! Dependency *cycles*, unknown dependencies and ambiguous ones are build
//! errors, which cannot be asserted from a passing test; they are covered by the
//! unit tests in `build.rs` (`subcrate::build`) instead.

use subcrate::subcrate;

// -- A plain dependency by ident ---------------------------------------------

#[subcrate]
pub mod base {
    pub struct Token(pub u32);

    pub fn helper() -> u32 {
        10
    }
}

// `base` is visible inside `derived` as `base`.
#[subcrate(base)]
pub mod derived {
    pub fn f() -> u32 {
        base::helper() + 1
    }

    pub fn make() -> base::Token {
        base::Token(5)
    }
}

#[test]
fn a_subcrate_can_call_the_subcrate_it_depends_on() {
    assert_eq!(derived::f(), 11);
}

#[test]
fn a_dependency_keeps_one_crate_identity_across_the_boundary() {
    // `derived::make()` returns *the same* `base::Token` the parent names: there
    // is one `base` crate, linked into both, not a copy per dependent.
    let token: base::Token = derived::make();
    assert_eq!(token.0, 5);
}

// A dependency is imported at the subcrate's crate root (`use ::<crate> as base;`),
// so it can be re-exported, and nested modules reach it as `crate::base` or
// `super::base`.
#[subcrate(base)]
pub mod reexporter {
    pub use base::Token;

    pub mod inner {
        pub fn f() -> u32 {
            super::base::helper() + 1
        }
    }

    pub fn g() -> u32 {
        crate::base::helper() + 2
    }
}

#[test]
fn a_dependency_is_importable_at_the_crate_root_and_reachable_from_nested_modules() {
    // `reexporter::Token` *is* `base::Token`.
    let token: base::Token = reexporter::Token(1);
    assert_eq!(token.0, 1);
    assert_eq!(reexporter::inner::f(), 11);
    assert_eq!(reexporter::g(), 12);
}

// The import is `extern crate … as …;` instead of a `use` in edition 2015.
#[subcrate(edition = "2015", base)]
pub mod legacy {
    pub fn f() -> u32 {
        base::helper() + 3
    }
}

#[test]
fn a_dependency_links_in_edition_2015_too() {
    assert_eq!(legacy::f(), 13);
}

// -- A chain, declared against the grain of the compile order ----------------

// `top <- mid <- low` (read: `top` uses `mid`, which uses `low`), declared in the
// reverse of the order they must be compiled in, so the order cannot be an
// accident of source position.
#[subcrate(mid)]
pub mod top {
    pub fn value() -> u32 {
        mid::value() * 2
    }
}

#[subcrate(low)]
pub mod mid {
    pub fn value() -> u32 {
        low::value() + 5
    }
}

#[subcrate]
pub mod low {
    pub fn value() -> u32 {
        1
    }
}

#[test]
fn a_chain_of_dependencies_compiles_in_topological_order() {
    assert_eq!(low::value(), 1);
    assert_eq!(mid::value(), 6);
    assert_eq!(top::value(), 12);
}

// -- Diamond: two subcrates sharing a dependency, joined by a third ----------

#[subcrate(base)]
pub mod left {
    pub fn f() -> u32 {
        base::helper() + 1
    }
}

#[subcrate(base)]
pub mod right {
    pub fn f() -> u32 {
        base::helper() + 2
    }
}

#[subcrate(left, right)]
pub mod join {
    pub fn f() -> u32 {
        left::f() * 100 + right::f()
    }
}

#[test]
fn a_diamond_of_dependencies_resolves() {
    assert_eq!(join::f(), 1112);
}

// -- Paths and aliases ---------------------------------------------------------

pub mod nested {
    use subcrate::subcrate;

    #[subcrate]
    pub mod leaf {
        pub fn n() -> u32 {
            3
        }
    }
}

// A module path names the subcrate; the dependency is visible under its last
// segment (`leaf`).
#[subcrate(nested::leaf)]
pub mod by_path {
    pub fn f() -> u32 {
        leaf::n() + 1
    }
}

// `crate::` is just the crate root, so this is the same path.
#[subcrate(crate::nested::leaf)]
pub mod by_crate_path {
    pub fn f() -> u32 {
        leaf::n() + 2
    }
}

// `alias = path` renames the dependency inside the subcrate.
#[subcrate(renamed = base, other = nested::leaf)]
pub mod aliased {
    pub fn f() -> u32 {
        renamed::helper() + other::n()
    }
}

#[test]
fn dependencies_resolve_by_module_path() {
    assert_eq!(by_path::f(), 4);
    assert_eq!(by_crate_path::f(), 5);
}

#[test]
fn a_dependency_can_be_renamed() {
    assert_eq!(aliased::f(), 13);
}

// -- Proc-macro subcrates ------------------------------------------------------

#[subcrate(proc_macro)]
pub mod answer_macro {
    use proc_macro::TokenStream;

    #[proc_macro]
    pub fn answer(_: TokenStream) -> TokenStream {
        "42".parse().unwrap()
    }
}

// A library subcrate invoking a proc-macro subcrate.
#[subcrate(answer_macro)]
pub mod uses_macro {
    pub fn f() -> u32 {
        answer_macro::answer!() + 1
    }
}

#[subcrate]
pub mod seven_text {
    pub fn text() -> &'static str {
        "7"
    }
}

// A proc-macro subcrate calling into a library subcrate while it expands.
#[subcrate(proc_macro, seven_text)]
pub mod seven_macro {
    use proc_macro::TokenStream;

    #[proc_macro]
    pub fn seven(_: TokenStream) -> TokenStream {
        seven_text::text().parse().unwrap()
    }
}

#[test]
fn a_library_subcrate_can_use_a_proc_macro_subcrate() {
    assert_eq!(uses_macro::f(), 43);
}

#[test]
fn a_proc_macro_subcrate_can_use_a_library_subcrate() {
    assert_eq!(seven_macro::seven!(), 7);
}
