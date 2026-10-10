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
   `Cargo.toml`, and no recursive Cargo for the subcrates themselves. (Only a
   subcrate that uses its owner's dependencies, below, has Cargo build those
   first, in a separate target directory.)
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

- `#[subcrate(proc_macro)]` (or `proc_macro = true`) — compile the module as a
  `proc-macro` crate.
- `#[subcrate(edition = "2018")]` — compile just this subcrate with a specific
  edition instead of the package's.
- Crate-root inner attributes written in the body (`#![no_std]`, `#![allow(…)]`,
  …) are applied to the subcrate's crate root.
- A subcrate body may use `mod foo;` to pull in other files (resolved like
  ordinary Rust modules, `#[path]` included); the build helper inlines them
  before compiling, and edits to those files trigger a rebuild. This form needs
  **nightly** Rust — a non-inline `mod foo;` in proc-macro input is unstable
  (`E0658`) on stable. Inline `mod foo { … }` works everywhere, including on the
  1.56 MSRV.

### Subcrates that depend on subcrates

A subcrate can link against other `#[subcrate]` modules of the same package.
List them among the attribute's arguments, as the path of the module:

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

#[subcrate(base, nested::leaf)]   // `base` and `leaf` are visible inside `derived`
pub mod derived {
    pub fn f() -> u32 { base::helper() + leaf::n() }
}

#[subcrate(h = base)]             // `alias = path` renames the dependency
pub mod renamed {
    pub fn f() -> u32 { h::helper() }
}
```

- A bare path (`base`, `nested::leaf`) makes the sibling subcrate available under
  the path's last segment; `alias = path` makes it available under `alias`
  instead. `proc_macro` is the only reserved bare name.
- Under the hood the dependency is passed to `rustc` as `--extern <its internal
  crate name>=<artifact>`, and `use ::<internal name> as <alias>;` is added at
  the subcrate's crate root (`extern crate … as …;` in edition 2015). That
  import is a build-time detail: it is not part of the subcrate's name. Since it
  sits at the crate root, a nested module of the subcrate reaches the dependency
  as `crate::alias` or `super::alias`, and the subcrate can re-export it
  (`pub use alias::Thing;`).
- The path is the module's path from its crate root (a leading `crate::` is
  allowed). A lone ident finds the subcrate with that name wherever it is nested;
  a full path picks among same-named ones. If a name is still ambiguous the
  subcrate under the same crate root (the same `src/lib.rs`, or the same
  `tests/foo.rs`) wins; otherwise, or if nothing matches, the build fails with a
  message saying so. Dependencies may cross crate roots (a `tests/` subcrate can
  depend on a `src/lib.rs` one).
- Subcrates are compiled **in dependency order** (a topological sort, done with
  [`safegraph`](https://docs.rs/safegraph)), and a dependency cycle is a build
  error that names the cycle (`subcrate dependency cycle: a -> b -> a`).
- Dependencies are part of a subcrate's identity, and changing a dependency's
  body recompiles its dependents (the artifact path they are linked against
  embeds the dependency's content hash).
- A library subcrate may depend on a `proc_macro` subcrate and invoke its macros
  (`name::mac!()`). A `proc_macro` subcrate may depend on library subcrates too
  (the macro can call them as it expands), except when cross-compiling: a library
  subcrate is built for the target, but a proc-macro runs on the host.
- A bare path that matches no sibling subcrate is a build error. To depend on a
  crate the package itself depends on, write `::name` (next section).

### Subcrates that use the owner's dependencies, or the owner itself

A subcrate may also use the crates the package that owns it depends on, and the
owner crate itself (the package's own library). A leading `::` marks them:

```rust ignore
// `serde` is a `[dependencies]` entry of this package.
#[subcrate(::serde)]
pub mod uses_serde {
    pub fn f() { let _ = serde::de::IgnoredAny; }
}

#[subcrate(s = ::serde)]          // `alias = ::name` renames it, as for subcrates
pub mod renamed { /* … `s::…` … */ }

// In `tests/foo.rs` (or `src/main.rs`, `src/bin/…`, `examples/…`): the package's own library.
#[subcrate(::my_package)]
pub mod uses_the_owner {
    pub fn f() -> u32 { my_package::something() }
}
```

- `::name` is the name the package's own code uses for a dependency of this
  package (a dash in the name is an underscore: `::proc_macro2` for `proc-macro2`)
  or the name of the package itself (`CARGO_PKG_NAME`, dashes as underscores). A
  name that is neither is a build error. It is visible inside the subcrate under
  `name` (or the `alias`), imported at the subcrate's crate root exactly like a
  subcrate dependency.
- **Renamed dependencies** work: with `myleaf = { package = "leaf", path = "../leaf"
  }` in `[dependencies]`, `#[subcrate(::myleaf)]` links the crate of the package
  `leaf` under the name `myleaf`, as the package's own code sees it. The build
  script finds the dependency by its key in `Cargo.toml` (`[dependencies]` first,
  then `[dev-dependencies]` and `[build-dependencies]`, also under `[target.…]`,
  and — for `name = { workspace = true }` — `[workspace.dependencies]`), takes the
  real package from its `package` field (else the key), builds that, and finds
  its crate by the package's name. (Cargo does not let a package list one package
  under two names.)
- **Several versions** of a crate in the dependency graph work too. `cargo build -p
  serde` is ambiguous then, so the build script takes the version the package
  resolves to from the `Cargo.lock` and builds `-p serde:1.0.200` (the
  `name:version` pkgid spec, which every Cargo accepts, Cargo 1.56 included;
  `name@version` is newer and rejected by Cargo 1.56), and picks that version's
  artifacts out of what Cargo built. A name that is not ambiguous stays a plain
  `-p name`. If the package itself depends on two versions of one package (the usual
  reason for renaming: `dup = "2"` and `dup_old = { package = "dup", version = "1" }`),
  the `version` requirement — or the version at the `path` — declared for each name
  says which is meant.
- The crate really is the owner's: Cargo builds it with the same versions and
  features as for the package, and the subcrate links against that build, so a
  type from it means the same thing in the subcrate and in the owner.
- **How:** a subcrate is compiled by the build script, before Cargo builds the
  package's dependencies, so the build script asks Cargo to build the ones that are
  needed — one nested `cargo build -p <dep> … [-p <owner>] --lib --offline
  --target-dir <profile dir>/subcrate-owner-deps/<package>/<rustc> [--release]`, run from
  the package's manifest directory. The separate target directory is essential: the
  build in progress holds the lock on the main one. It is shared by every
  configuration of the package (`cargo build`, `cargo test`, `cargo check`, … each
  have an `OUT_DIR` of their own), so the crates are compiled once, not once per
  configuration; it is per package, per profile (`target/debug`,
  `target/release`, `target/<triple>/debug`, …) and per compiler (its release and
  commit, so toolchains do not overwrite each other's crates), and persists between
  builds. The crates (and their own dependencies)
  are then passed to `rustc` as `--extern <name>=<rlib>` *and* `--extern
  <name>=<rmeta>`, with `-L` for the directories holding them. A package that uses
  none of this builds nothing extra, and only the crates a subcrate names are built
  (with their own dependencies) — the owner crate only if one uses `::<owner>`.
- **The owner crate** is the package's *library*, built a second time (running
  this build script again in the nested build, with `SUBCRATE_NESTED` set, which
  makes it compile only the subcrates the library uses and never start another
  owner build). A subcrate that is part of the library (`src/lib.rs` and the files it
  reaches) cannot use the owner crate — the library would depend on itself — and
  that is a build error; one in `tests/`, `src/main.rs`, `src/bin/`, `examples/` or
  `benches/` can. A package with no library has no owner crate to use.
- The subcrates are compiled after the owner's own subcrates (the library is made
  of them), and recompiled when the crates they link against are rebuilt; the build
  script reruns when a path dependency's sources, `Cargo.lock` or `Cargo.toml`
  change.
- **Limitations:** the crate must be an ordinary library crate (not a proc-macro),
  and `::name` names the crate itself, not a path into it (`::serde::de` is
  an error). A dev-dependency is built as `cargo build` would build it, so its
  features differ from the test build's if (resolver 2) only a dev-dependency edge
  enables them. Cargo and the build script must agree on the build, so a custom profile
  is built as `dev` or `release` (a build script is only told `debug` or `release`)
  and, when cross-compiling, a proc-macro subcrate cannot use these crates (they
  are built for the target, a proc-macro runs on the host). The nested build runs
  from the package's own directory and `--offline`, so it expects to be part of a
  workspace with its `Cargo.lock` and sources already in place (a package built
  as someone's *dependency* resolves its own versions instead). Under `cargo check` and
  clippy, Cargo builds different copies of the dependencies than `cargo build` does,
  so a *type* from one is not the same type as the subcrate's there: code that
  passes such a value across the boundary compiles under `cargo build`/`test`, but
  not under `cargo check`. If the package itself depends on two versions of one
  package and gives the dependency no `version` (or `path`) that tells them apart, or
  on two packages of one name and version from different sources, the lock file does
  not say which `::name` means, and the build stops with an error. Building the
  owner crate compiles its build-dependencies (`subcrate` itself, `syn`, …) once in
  the shared directory above, which is most of that directory's size; that is
  inherent in building the library separately.

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
  consuming crate (each `tests/` file is its own crate linking `example`),
  including one that depends on another subcrate.
- `examples/example/tests/dependencies.rs` — subcrates depending on subcrates:
  chains, diamonds, paths, aliases, and proc-macro subcrates on either side.
- `examples/example/tests/owner.rs` — subcrates using the owner's dependencies
  (`::leaf`, with its own dependency `leaf-base`), a renamed one (`::alt` for the
  package `leaf-alt`) and the owner crate itself (`::example`).
- `core/tests/args.rs` and the unit tests in `build.rs` — `#[subcrate(...)]`
  argument parsing, and dependency resolution/ordering including the build-error
  cases (cycles, unknown or ambiguous dependencies, the owner crate used from its
  own library, a `::name` that is not a dependency), and the reading of renamed
  dependencies from `Cargo.toml` and of multiple versions from `Cargo.lock`.

Code generation throughout uses `template-quote` (<https://docs.rs/template-quote>).

## MSRV

1.56. The committed `Cargo.lock` pins dependency versions that build on 1.56;
building with a newer toolchain works too. (Cargo 1.56 is not MSRV-aware, so if
you regenerate the lockfile on a newer toolchain you may need to pin a few
transitive crates back down.)

## Known limitations

- Subcrate bodies are compiled against the standard library (and `proc_macro`
  for `proc_macro = true`), plus whatever they name as a dependency: sibling
  subcrates, and the owner's dependencies or the owner crate (`::name`, with the
  limitations above).
- `cargo doc` of the subcrate-defining crate works (subcrates are rendered via a
  `#[cfg(doc)]` stand-in, since rustdoc is not given the build-script search
  path). However, a *downstream* crate's `cargo doc` cannot resolve a
  dependency's subcrate-backed items referenced in documented code — Cargo
  forwards the search path to `rustc` but not to `rustdoc`. Guard such
  references with `#[cfg(not(doc))]` if you need downstream docs to build.
- That `cfg(doc)` stand-in imports a subcrate's dependencies at the top of its
  module, mirroring the real crate-root import, but only when the dependency is
  under the same crate root (a dependency in another crate root has no
  `crate::` path to import).

## License

Licensed under the [MIT license](./LICENSE).
