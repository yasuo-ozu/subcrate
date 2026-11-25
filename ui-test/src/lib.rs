//! A package with **no** build script, to exercise the no-`build.rs` `should_error`
//! path: `#[subcrate(should_error …)]` modules in `tests/ui.rs` are checked at test
//! time by [`subcrate::ui`]. The library itself is empty on purpose — it carries no
//! `subcrate` dependency of any kind, only the dev-dependency the tests use.
