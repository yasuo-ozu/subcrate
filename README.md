<p align="center">
  <img src="https://raw.githubusercontent.com/yasuo-ozu/subcrate/main/logo.png" width="168" alt="subcrate logo">
</p>

# subcrate [![Latest Version]][crates.io] [![Documentation]][docs.rs] [![CI]][ci-workflow] [![MSRV]][msrv-workflow]

[Latest Version]: https://img.shields.io/crates/v/subcrate.svg
[crates.io]: https://crates.io/crates/subcrate
[Documentation]: https://img.shields.io/docsrs/subcrate
[docs.rs]: https://docs.rs/subcrate/latest/subcrate/
[CI]: https://github.com/yasuo-ozu/subcrate/actions/workflows/ci.yml/badge.svg
[ci-workflow]: https://github.com/yasuo-ozu/subcrate/actions/workflows/ci.yml
[MSRV]: https://img.shields.io/crates/msrv/subcrate.svg
[msrv-workflow]: https://github.com/yasuo-ozu/subcrate/actions/workflows/msrv.yml

**Write real, separately compiled Rust crates as inline modules.**

Put `#[subcrate]` on an inline module and its body is built as its own crate —
with its own crate root, its own `crate::`, and its own type identities and
orphan rules. The module keeps its name, place, and visibility in the crate
around it. You add no extra Cargo packages and no separate source files, and
everything runs under the usual `cargo build`, `check`, `test`, and `doc`.

```rust ignore
use subcrate::subcrate;

// A proc-macro crate, written inline.
#[subcrate(proc_macro = true)]
pub mod my_macro {
    use proc_macro::TokenStream;
    #[proc_macro]
    pub fn mac(_: TokenStream) -> TokenStream { "42".parse().unwrap() }
}

// An ordinary library crate.
#[subcrate]
pub mod my_crate {
    pub fn calc() -> usize { 32 }
}

// One subcrate can use another. Name it as an argument, and it is in scope
// inside under its last path segment (here, `my_crate`).
#[subcrate(my_crate)]
pub mod doubled {
    pub fn calc() -> usize { my_crate::calc() * 2 }
}

// `#[subcrate]` works in nested modules too, and the path stays the same.
pub mod nested {
    use subcrate::subcrate;
    #[subcrate]
    pub mod inner {
        pub fn answer() -> usize { 42 }
    }
}

pub fn calculate() -> usize {
    doubled::calc() + nested::inner::answer() // 64 + 42
}
```

## Macros and ordinary items in one crate

A `proc-macro` crate can export only macros — no functions, types, or constants
for other crates to use. `subcrate` lifts that limit. A normal library crate can
hold proc-macros (as `#[subcrate(proc_macro = true)]` modules) next to its
ordinary items, and even re-export a macro from its own root:

```rust ignore
#[subcrate(proc_macro = true)]
pub mod macros {
    use proc_macro::TokenStream;
    #[proc_macro]
    pub fn mac(_: TokenStream) -> TokenStream { "42".parse().unwrap() }
}

pub use macros::mac;             // now `thiscrate::mac!()` works...
pub fn ordinary() -> usize { 7 } // ...next to ordinary items, in one crate.
```

Without `subcrate` this takes two packages: a proc-macro crate, and a normal
crate that re-exports it.

## Setup

Three pieces, all required.

**`Cargo.toml`** — `subcrate` is both a normal and a build dependency:

```toml
[dependencies]
subcrate = "0.1"

[build-dependencies]
subcrate = "0.1"
```

**`build.rs`** — one required call:

```rust ignore
fn main() {
    subcrate::build::compile().unwrap();
}
```

**`src/lib.rs`** (or `main.rs`, a test, …) — write `use subcrate::subcrate;` and
put `#[subcrate]` on inline modules.

If the build helper never runs, `#[subcrate]` gives a clear compile error instead
of a confusing linker failure.

A crate that needs only the attribute, not `build::compile`, can leave the build
machinery out of its own build with `default-features = false`.

## How it works

`subcrate::build::compile()` runs from your build script. It:

1. Finds the package's source files from its crate roots (`src/lib.rs`,
   `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`, `benches/*.rs`, `examples/*.rs`)
   and follows `mod foo;` and `#[path = "…"]`. It starts from
   `CARGO_MANIFEST_DIR`, so it also works from a published, extracted package.
2. Parses them and finds every `#[subcrate]` inline module.
3. Compiles each one by calling `rustc` directly (from `$RUSTC`) and feeding the
   module body in on **stdin**. There are no temporary `.rs` files, no
   per-subcrate `Cargo.toml`, and no recursive Cargo for the subcrates. (Cargo is
   used only to build a subcrate's owner dependencies, described below.)
4. Puts the compiled crates on the library search path and writes a small
   manifest that the attribute macro reads to confirm the helper ran.

The `#[subcrate]` attribute then replaces the module with a binding to that real
crate — roughly `pub extern crate __subcrate_inner_<hash> as inner;` — keeping the
original visibility and attributes. Each subcrate's internal name is a hash of its
identifier and body. The macro and the build helper compute it separately and
always agree, and it gives incremental reuse for free: an unchanged body is not
recompiled, and a changed body gets a fresh crate while the old one is pruned.

### Same configuration as the owner

A library subcrate is part of the final build, so it is compiled to match the
package's configuration: its **edition**, **opt-level**, **panic strategy**,
**debug-assertions and overflow-checks**, the **target triple** (when
cross-compiling), the package's **enabled feature cfgs** (so `#[cfg(feature =
"…")]` works inside a subcrate), and **`RUSTFLAGS`** / `[build] rustflags` /
target flags. The reuse stamp records the full flag set, so a config change
rebuilds rather than reuses a stale crate.

A proc-macro subcrate is a host artifact, so it is built with the host's default
configuration (no `--target`, panic, or `RUSTFLAGS`) — just as Cargo builds a
normal proc-macro crate.

### Per-subcrate options

- `#[subcrate(proc_macro)]` (or `proc_macro = true`) — build the module as a
  `proc-macro` crate.
- `#[subcrate(edition = "2018")]` — build this one subcrate with a given edition
  instead of the package's.
- Crate-root inner attributes in the body (`#![no_std]`, `#![allow(…)]`, …) are
  applied to the subcrate's crate root.
- A subcrate body may use `mod foo;` to pull in other files (resolved like
  ordinary Rust modules, `#[path]` included). The build helper inlines them
  before compiling, and editing one triggers a rebuild. This form needs
  **nightly** Rust: a non-inline `mod foo;` in proc-macro input is unstable
  (`E0658`) on stable. Inline `mod foo { … }` works everywhere, down to the 1.56
  MSRV.

### Subcrates that depend on subcrates

A subcrate can link against other `#[subcrate]` modules of the same package. List
them as arguments, by module path:

```rust ignore
#[subcrate]
pub mod base {
    pub fn helper() -> u32 { 10 }
}

pub mod nested {
    use subcrate::subcrate;
    #[subcrate]
    pub mod leaf { pub fn n() -> u32 { 3 } }
}

// `base` and `leaf` are in scope inside `derived`.
#[subcrate(base, nested::leaf)]
pub mod derived {
    pub fn f() -> u32 { base::helper() + leaf::n() }
}

// `alias = path` renames the dependency.
#[subcrate(h = base)]
pub mod renamed {
    pub fn f() -> u32 { h::helper() }
}
```

- A bare path (`base`, `nested::leaf`) brings the sibling in under its last
  segment; `alias = path` brings it in under `alias`. `proc_macro` is the only
  reserved bare name.
- The path runs from the sibling's crate root. A lone name finds that subcrate
  wherever it is nested; a full path picks among same-named ones. Dependencies may
  cross crate roots (a `tests/` subcrate can depend on a `src/lib.rs` one).
- The dependency is in scope at the subcrate's crate root, so a nested module
  reaches it as `crate::alias`, and the subcrate can re-export it
  (`pub use alias::Thing;`).
- Subcrates compile **in dependency order**; a cycle is a build error that names
  it. Changing a dependency's body recompiles its dependents.
- A library subcrate may depend on a `proc_macro` subcrate and call its macros; a
  `proc_macro` subcrate may depend on library subcrates too — except when
  cross-compiling (a library subcrate is built for the target, a proc-macro for the
  host).
- To depend on a crate the package itself depends on, write `::name` (next
  section).

### Subcrates that use the owner's dependencies, or the owner itself

A subcrate can also use the crates the owning package depends on, and the owner
crate itself (the package's own library). A leading `::` marks them:

```rust ignore
// `syn` is a `[dependencies]` entry of this package.
#[subcrate(::syn)]
pub mod parse {
    pub fn item_count(src: &str) -> usize {
        syn::parse_file(src).unwrap().items.len()
    }
}

// Mix a sibling subcrate and an owner dependency in one subcrate.
#[subcrate(my_crate, ::syn)]
pub mod analyze {
    pub fn score(src: &str) -> usize {
        syn::parse_file(src).unwrap().items.len() + my_crate::calc()
    }
}

// `alias = ::name` renames it, as for a subcrate dependency.
#[subcrate(s = ::syn)]
pub mod renamed { /* … use `s::…` … */ }

// In `tests/foo.rs` (or `src/main.rs`, `src/bin/…`, `examples/…`):
// use the package's own library.
#[subcrate(::my_package)]
pub mod uses_the_owner {
    pub fn f() -> u32 { my_package::something() }
}
```

`::name` is the name the package's own code uses for a dependency (a dash becomes
an underscore: `::proc_macro2` for `proc-macro2`), or the package's own name. It
is in scope inside the subcrate under `name` (or the `alias`). The crate really is
the owner's — Cargo builds it with the same versions and features as for the
package — so a type from it means the same thing in the subcrate and in the owner.

Renamed dependencies (`myleaf = { package = "leaf", … }`) and graphs with several
versions of one crate both work; the build helper resolves each `::name` through
`Cargo.toml` and `Cargo.lock`, as the package itself does.

The **owner crate** is the package's own library. A subcrate in the library cannot
use it (the library would depend on itself), but one in `tests/`, `src/main.rs`,
`src/bin/`, `examples/`, or `benches/` can.

Under the hood the build helper has Cargo build the needed crates once, into a
separate target directory, and links the subcrate against them. There are a few
limitations here — proc-macro crates, `cargo check` type identity, cross-compiling,
and more; see the `subcrate::build` API docs for the full list.

### Crate boundaries are real

Inside a subcrate, `crate::` means the subcrate itself and `super::` cannot reach
the module around it. The parent's private items stay inaccessible, the orphan
rules apply across the boundary, and the subcrate's types and traits have its own
crate identity. Publicly exported subcrates work for downstream packages: Cargo
forwards the build-script search path to dependents, so `cargo build`/`test`
resolve them with no extra wiring.

## Known limitations

- A subcrate body is compiled against the standard library (and `proc_macro` for
  `proc_macro = true`), plus whatever it names as a dependency: sibling subcrates,
  and the owner's dependencies or the owner crate (`::name`, with the limits in the
  `subcrate::build` API docs).
- `cargo doc` of the subcrate-defining crate works: subcrates are rendered through
  a `#[cfg(doc)]` stand-in, because rustdoc is not given the build-script search
  path. But a *downstream* crate's `cargo doc` cannot resolve a dependency's
  subcrate-backed items used in documented code — Cargo forwards the search path to
  `rustc`, not to `rustdoc`. Guard such references with `#[cfg(not(doc))]` if you
  need downstream docs to build.
- The `cfg(doc)` stand-in imports a subcrate's dependencies at the top of its
  module, mirroring the real crate-root import — but only for a dependency under
  the same crate root (one in another crate root has no `crate::` path to import).

## Related work

Defining extra crates inline has been [proposed for Rust itself][inline-crates]
(see the [lang-team discussion][lang-team-139]), but it is not available, and that
proposal leaves proc-macros out — the case where the separate-crate split hurts
most. `subcrate` works today on stable and handles proc-macros. Other crates ease
the two-crate proc-macro split in different ways, such as [`include_proc_macro`]
for pulling macro sources in from arbitrary paths, but none give an inline module
a real, separate crate identity.

[inline-crates]: https://blog.yoshuawuyts.com/inline-crates
[lang-team-139]: https://github.com/rust-lang/lang-team/issues/139
[`include_proc_macro`]: https://docs.rs/include_proc_macro

## License

Licensed under the [MIT license](./LICENSE).
