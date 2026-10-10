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

**Define real, independently compiled Rust crates with inline module syntax.**

Annotating an inline module with `#[subcrate]` compiles its body as a
*genuinely separate crate* — with its own crate root, its own `crate::`, and its
own type identities and orphan-rule scope — while keeping the module's original
name, location, and visibility in the containing crate. No extra Cargo packages,
no hand-maintained source files, and nothing beyond ordinary `cargo build` /
`check` / `test` / `doc`.

```rust ignore
use subcrate::subcrate;

#[subcrate(proc_macro = true)]
pub mod my_macro {
    use proc_macro::TokenStream;
    #[proc_macro]
    pub fn mac(_: TokenStream) -> TokenStream { "42".parse().unwrap() }
}

#[subcrate]
pub mod my_crate {
    pub fn calc() -> usize { 32 }
}

pub mod nested {
    use subcrate::subcrate;
    #[subcrate]
    pub mod inner {
        pub fn answer() -> usize { 42 }
    }
}

pub fn calculate() -> usize {
    my_crate::calc() + nested::inner::answer()
}
```

## One crate, both macros and ordinary items

A `proc-macro` crate (`--crate-type proc-macro`) may export **nothing but
macros** — no functions, types, or constants usable by other crates. `subcrate`
lifts that restriction: an ordinary library crate can carry proc-macros (as
`#[subcrate(proc_macro = true)]` modules) right beside its normal items, and even
re-export a macro to its own root:

```rust ignore
#[subcrate(proc_macro = true)]
pub mod macros {
    use proc_macro::TokenStream;
    #[proc_macro]
    pub fn mac(_: TokenStream) -> TokenStream { "42".parse().unwrap() }
}

pub use macros::mac;            // now `thiscrate::mac!()` works…
pub fn ordinary() -> usize { 7 } // …next to ordinary items, in one crate.
```

Without `subcrate` this needs two packages (a proc-macro crate plus a normal
crate that re-exports it).

## Setup

Three pieces, all required:

**`Cargo.toml`** — `subcrate` is both a normal and a build dependency:

```toml
[dependencies]
subcrate = "0.1"

[build-dependencies]
subcrate = "0.1"
```

**`build.rs`** — one mandatory call:

```rust ignore
fn main() {
    subcrate::build::compile().unwrap();
}
```

**`src/lib.rs`** (or `main.rs`, a test, …) — `use subcrate::subcrate;` and
annotate inline modules with `#[subcrate]`.

If the build helper is not invoked, `#[subcrate]` produces a clear compile error
instead of a confusing linker failure.

A crate that only needs the attribute (not `build::compile`) can drop the build
machinery from its own compilation with `default-features = false`.

## How it works

`subcrate::build::compile()` runs from your build script and:

1. Discovers the package's source files from its crate roots (`src/lib.rs`,
   `src/main.rs`, `src/bin/*.rs`, `tests/*.rs`, `benches/*.rs`, `examples/*.rs`),
   following `mod foo;` and `#[path = "…"]` declarations. Discovery is rooted at
   `CARGO_MANIFEST_DIR`, so it works from an extracted/published package too.
2. Parses them and finds every `#[subcrate]` inline module.
3. Compiles each one by invoking `rustc` directly (from `$RUSTC`), feeding the
   module body in on **stdin** — no temporary `.rs` files, no per-subcrate
   `Cargo.toml`, and no recursive Cargo.
4. Puts the compiled artifacts on the library search path and records a small
   manifest the procedural macro reads to confirm it ran.

The `#[subcrate]` attribute then replaces the module with a binding to that real
crate, roughly `pub extern crate __subcrate__inner__<hash> as inner;`, preserving
the original visibility and attributes. Each subcrate's internal name is a hash
of its identifier and body, which the macro and the build helper compute
independently and always agree on — and which gives incremental reuse for free
(unchanged bodies are not recompiled; changed bodies get a fresh artifact and the
stale one is pruned).

### Same configuration as the owner

Library subcrates are part of the final artifact, so they are compiled to match
the owning package's configuration: its **edition**, **opt-level**, **panic
strategy**, **debug-assertions/overflow-checks**, the **target triple** (when
cross-compiling), the owner's **enabled feature cfgs** (so `#[cfg(feature =
"…")]` works inside a subcrate), and **`RUSTFLAGS`** / `[build] rustflags` /
target flags. The reuse stamp records the full flag set, so a configuration
change forces a rebuild rather than reusing a stale artifact.

Procedural-macro subcrates are host artifacts and are deliberately built with the
host's default configuration (no target `--target`, panic, or `RUSTFLAGS`),
exactly as Cargo builds a normal proc-macro crate.

### Per-subcrate options

- `#[subcrate(proc_macro = true)]` — compile the module as a `proc-macro` crate.
- `#[subcrate(edition = "2018")]` — compile just this subcrate with a specific
  edition instead of the package's.
- Crate-root inner attributes written in the body (`#![no_std]`, `#![allow(…)]`,
  …) are applied to the subcrate's crate root.
- A subcrate body may use `mod foo;` to pull in other files (resolved like
  ordinary Rust modules, `#[path]` included); the build helper inlines them
  before compiling, and edits to those files trigger a rebuild. This needs a
  Rust newer than the 1.56 MSRV, since a non-inline `mod foo;` in proc-macro
  input was unstable on 1.56 (`E0658`); inline `mod foo { … }` works everywhere.

### Crate boundaries are real

Inside a subcrate, `crate::` refers to the subcrate itself and `super::` cannot
reach the containing module; private items of the parent are inaccessible, the
orphan rules apply across the boundary, and the subcrate's types/traits have its
own crate identity. Publicly exported subcrates are usable by downstream
packages — Cargo forwards the build-script search path to dependents, so
`cargo build`/`test` resolve them with no extra wiring.

## Tests

All tests are integration tests under `tests/` directories, run by
`cargo test --workspace`:

- `tests/build_api.rs` — the root package's tests for the `build` API error
  paths (it cannot host `#[subcrate]` tests, because a package may not depend on
  itself to run its own build helper).
- `examples/example/tests/integration.rs` — the full pipeline: library, nested,
  and proc-macro subcrates resolved by their original paths, a crate exporting
  both a proc-macro and normal items, and a `#[subcrate]` declared inside the
  test file (proving `tests/` discovery and that `crate::` resolves to the
  subcrate's own root).
- `examples/example/tests/consumer.rs` — the same subcrates used from a separate
  consuming crate (each `tests/` file is its own crate linking `example`).

Code generation throughout uses `template-quote` (<https://docs.rs/template-quote>).

## MSRV

1.56. The committed `Cargo.lock` pins dependency versions that build on 1.56;
building with a newer toolchain works too. (Cargo 1.56 is not MSRV-aware, so if
you regenerate the lockfile on a newer toolchain you may need to pin a few
transitive crates back down.)

## Known limitations

- Subcrate bodies are compiled against the standard library (and `proc_macro`
  for `proc_macro = true`). Pulling the parent's own dependencies into a
  subcrate is out of scope.
- `cargo doc` of the subcrate-defining crate works (subcrates are rendered via a
  `#[cfg(doc)]` stand-in, since rustdoc is not given the build-script search
  path). However, a *downstream* crate's `cargo doc` cannot resolve a
  dependency's subcrate-backed items referenced in documented code — Cargo
  forwards the search path to `rustc` but not to `rustdoc`. Guard such
  references with `#[cfg(not(doc))]` if you need downstream docs to build.

## License

Licensed under the [MIT license](./LICENSE).
