//! A sibling `#[subcrate]` dependency is bound into the dependent subcrate's
//! *extern prelude* (via `extern crate … as …;`), not just as a `use` alias. So
//! the dependent can reach it with a leading `::` and re-export it with `pub use`
//! — neither of which worked when the binding was a plain `use` alias (issue #4).

use subcrate::subcrate;

#[subcrate]
pub mod provider {
    pub struct Thing(pub u32);
    pub fn make() -> Thing {
        Thing(7)
    }
}

#[subcrate(provider)]
pub mod referrer {
    // Re-export through the alias: a private `use` alias would be `E0365` here.
    pub use provider::Thing;

    // Reach the sibling by a leading-`::` (extern prelude) path.
    pub fn get() -> ::provider::Thing {
        ::provider::make()
    }
}

#[test]
fn sibling_is_in_the_extern_prelude_and_re_exportable() {
    let thing: referrer::Thing = referrer::get();
    assert_eq!(thing.0, 7);
}
